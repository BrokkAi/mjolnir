// Port of clap_builder 4.6.0 src/builder/value_parser.rs

package clap

import (
	"math"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// BuilderValueParser is clap::builder::ValueParser: parse/validate argument
// values.
//
// Specified with Arg::value_parser.
//
// ValueParser defines how to convert a raw argument value into a validated
// and typed value for use within an application.
//
// See
//   - the ValueParser* functions (Rust's `value_parser!`) for automatically
//     selecting an implementation for a given type
//   - BuilderValueParserNew for additional TypedValueParser that can be used
type BuilderValueParser struct {
	inner builderAnyValueParser
}

// BuilderIntoValueParser is what Arg::value_parser accepts: Rust's
// `impl Into<ValueParser>`. A BuilderValueParser is one, and so is every
// typed parser of this package.
type BuilderIntoValueParser interface {
	IntoValueParser() BuilderValueParser
}

// IntoValueParser makes a ValueParser acceptable where one is asked for.
func (p BuilderValueParser) IntoValueParser() BuilderValueParser { return p }

// BuilderValueParserNew is ValueParser::new: custom parser for argument
// values.
//
// Pre-existing TypedValueParser implementations include:
//   - a function (BuilderValueParserFromFn)
//   - arrays of values (BuilderPossibleValuesParserNew)
//   - BuilderEnumValueParser
//   - BuilderPossibleValuesParser
//   - BuilderFalseyValueParser
//   - BuilderBoolishValueParser
//   - BuilderNonEmptyStringValueParser
func BuilderValueParserNew[T any](other BuilderTypedValueParser[T]) BuilderValueParser {
	return BuilderValueParser{inner: &builderTypedAdapter[T]{parser: other, id: utilAnyValueIdOf[T]()}}
}

// BuilderValueParserBool is ValueParser::bool: `bool` parser for argument
// values.
//
// See also:
//   - BuilderFalseyValueParser for different human readable bool
//     representations
//   - BuilderBoolishValueParser for different human readable bool
//     representations
func BuilderValueParserBool() BuilderValueParser {
	return BuilderValueParser{inner: builderValueParserInnerBool}
}

// BuilderValueParserString is ValueParser::string: `String` parser for
// argument values.
//
// See also:
//   - BuilderNonEmptyStringValueParser
func BuilderValueParserString() BuilderValueParser {
	return BuilderValueParser{inner: builderValueParserInnerString}
}

// BuilderValueParserOsString is ValueParser::os_string: `OsString` parser
// for argument values.
func BuilderValueParserOsString() BuilderValueParser {
	return BuilderValueParser{inner: builderValueParserInnerOsString}
}

// BuilderValueParserPathBuf is ValueParser::path_buf: `PathBuf` parser for
// argument values.
func BuilderValueParserPathBuf() BuilderValueParser {
	return BuilderValueParser{inner: builderValueParserInnerPathBuf}
}

var (
	builderValueParserInnerBool = &builderTypedAdapter[bool]{
		parser: BuilderBoolValueParser{}, id: utilAnyValueIdNamed[bool]("bool"), debug: "ValueParser::bool"}
	builderValueParserInnerString = &builderTypedAdapter[string]{
		parser: BuilderStringValueParser{}, id: utilAnyValueIdNamed[string]("alloc::string::String"), debug: "ValueParser::string"}
	builderValueParserInnerOsString = &builderTypedAdapter[string]{
		parser: BuilderOsStringValueParser{}, id: utilAnyValueIdNamed[string]("std::ffi::os_str::OsString"), debug: "ValueParser::os_string"}
	builderValueParserInnerPathBuf = &builderTypedAdapter[string]{
		parser: BuilderPathBufValueParser{}, id: utilAnyValueIdNamed[string]("std::path::PathBuf"), debug: "ValueParser::path_buf"}
)

// parseRef parses into a AnyValue.
//
// When `arg` is nil, an external subcommand value is being parsed.
func (p BuilderValueParser) parseRef(cmd *Command, arg *Arg, value string, source ParserValueSource) (any, *Error) {
	return p.inner.parseRef_(cmd, arg, value, source)
}

// TypeId is ValueParser::type_id: describes the content of the value this
// parser produces.
func (p BuilderValueParser) TypeId() utilAnyValueId {
	return p.inner.typeId()
}

// PossibleValues is ValueParser::possible_values: reflect on enumerated
// value properties.
//
// Error checking should not be done with this; it is mostly targeted at
// user-facing applications like errors and completion.
func (p BuilderValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return p.inner.possibleValues()
}

func (p BuilderValueParser) isSet() bool { return p.inner != nil }

// debugString is `impl Debug for ValueParser`.
func (p BuilderValueParser) debugString() string {
	return p.inner.debugString()
}

// The ranges Rust converts into a ValueParser:
// `impl From<std::ops::Range<i64>> for ValueParser` and its siblings.

// BuilderValueParserFromRange is `impl From<Range*<i64>> for ValueParser`:
// create an `i64` ValueParser from a range, such as `N..M`, `N..=M`, `N..`,
// `..M`, `..=M` or `..`.
//
// See BuilderRangedI64ValueParser for more control over the output type.
//
// See also BuilderRangedU64ValueParser.
func BuilderValueParserFromRange(value RangeBounds[int64]) BuilderValueParser {
	inner := BuilderRangedI64ValueParserNew[int64]().Range(value)
	return inner.IntoValueParser()
}

// builderAnyValueParser is the object-safe AnyValueParser: a helper to
// allow ValueParser to be type-erased.
type builderAnyValueParser interface {
	parseRef_(cmd *Command, arg *Arg, value string, source ParserValueSource) (any, *Error)
	typeId() utilAnyValueId
	possibleValues() rs.Option[[]BuilderPossibleValue]
	debugString() string
}

// builderTypedAdapter is `impl<T, P: TypedValueParser<Value = T>>
// AnyValueParser for P`.
type builderTypedAdapter[T any] struct {
	parser BuilderTypedValueParser[T]
	id     utilAnyValueId
	debug  string
}

func (a *builderTypedAdapter[T]) parseRef_(cmd *Command, arg *Arg, value string, source ParserValueSource) (any, *Error) {
	parsed, err := a.parser.ParseRef_(cmd, argOption(arg), value, source)
	if err != nil {
		return nil, errorFromAny(err)
	}
	return parsed, nil
}

func (a *builderTypedAdapter[T]) typeId() utilAnyValueId { return a.id }

func (a *builderTypedAdapter[T]) possibleValues() rs.Option[[]BuilderPossibleValue] {
	return a.parser.PossibleValues()
}

func (a *builderTypedAdapter[T]) debugString() string {
	if a.debug != "" {
		return a.debug
	}
	return "ValueParser::other(" + a.id.debugString() + ")"
}

func argOption(arg *Arg) rs.Option[*Arg] {
	if arg == nil {
		return rs.None[*Arg]()
	}
	return rs.Some(arg)
}

func argDisplayOr(arg rs.Option[*Arg], fallback string) string {
	if a, ok := arg.Get(); ok {
		return a.String()
	}
	return fallback
}

func argIgnoreCase(arg rs.Option[*Arg]) bool {
	if a, ok := arg.Get(); ok {
		return a.IsIgnoreCaseSet()
	}
	return false
}

// BuilderTypedValueParser is clap::builder::TypedValueParser: parse/validate
// argument values.
//
// As alternatives to implementing TypedValueParser,
//   - use BuilderValueParserFromFn with a function
//   - BuilderTypedValueParserMap or BuilderTypedValueParserTryMap to adapt
//     an existing TypedValueParser
//
// See the ValueParser* functions for automatically selecting an
// implementation for a given type.
//
// Rust gives every method but parse_ref a default body. A Go implementation
// gets them by embedding BuilderTypedValueParserDefaults, or writes the
// one-line methods that call BuilderTypedValueParserDefault*.
type BuilderTypedValueParser[T any] interface {
	// ParseRef parses the argument value.
	//
	// When `arg` is None, an external subcommand value is being parsed.
	ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (T, error)

	// ParseRef_ parses the argument value.
	//
	// When `arg` is None, an external subcommand value is being parsed.
	ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error)

	// Parse parses the argument value.
	//
	// When `arg` is None, an external subcommand value is being parsed.
	Parse(cmd *Command, arg rs.Option[*Arg], value string) (T, error)

	// Parse_ parses the argument value.
	//
	// When `arg` is None, an external subcommand value is being parsed.
	Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error)

	// PossibleValues reflects on enumerated value properties.
	//
	// Error checking should not be done with this; it is mostly targeted at
	// user-facing applications like errors and completion.
	PossibleValues() rs.Option[[]BuilderPossibleValue]
}

// BuilderTypedValueParserDefaultParseRef_ is the default body of
// TypedValueParser::parse_ref_.
func BuilderTypedValueParserDefaultParseRef_[T any](p BuilderTypedValueParser[T], cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return p.ParseRef(cmd, arg, value)
}

// BuilderTypedValueParserDefaultParse is the default body of
// TypedValueParser::parse.
func BuilderTypedValueParserDefaultParse[T any](p BuilderTypedValueParser[T], cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	return p.ParseRef(cmd, arg, value)
}

// BuilderTypedValueParserDefaultParse_ is the default body of
// TypedValueParser::parse_.
func BuilderTypedValueParserDefaultParse_[T any](p BuilderTypedValueParser[T], cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return p.Parse(cmd, arg, value)
}

// BuilderTypedValueParserDefaultPossibleValues is the default body of
// TypedValueParser::possible_values.
func BuilderTypedValueParserDefaultPossibleValues() rs.Option[[]BuilderPossibleValue] {
	return rs.None[[]BuilderPossibleValue]()
}

// BuilderTypedValueParserMap is TypedValueParser::map: adapt a
// TypedValueParser from one value to another.
func BuilderTypedValueParserMap[B, T any](p BuilderTypedValueParser[B], f func(B) T) *BuilderMapValueParser[B, T] {
	return &BuilderMapValueParser[B, T]{parser: p, f: f}
}

// BuilderTypedValueParserTryMap is TypedValueParser::try_map: adapt a
// TypedValueParser from one value to another.
func BuilderTypedValueParserTryMap[B, T any](p BuilderTypedValueParser[B], f func(B) (T, error)) *BuilderTryMapValueParser[B, T] {
	return &BuilderTryMapValueParser[B, T]{parser: p, f: f}
}

// builderFnValueParser is `impl<F, T, E> TypedValueParser for F where F:
// Fn(&str) -> Result<T, E>`.
type builderFnValueParser[T any] struct {
	f func(string) (T, error)
}

// BuilderValueParserFromFn makes a function a ValueParser: Rust's
// `impl TypedValueParser for F where F: Fn(&str) -> Result<T, E>`, which is
// what `#[arg(value_parser = function)]` and `.value_parser(function)` use.
func BuilderValueParserFromFn[T any](f func(string) (T, error)) BuilderValueParser {
	return BuilderValueParserNew[T](&builderFnValueParser[T]{f: f})
}

func (p *builderFnValueParser[T]) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	var zero T
	if !validUtf8(value) {
		return zero, errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	parsed, err := p.f(value)
	if err != nil {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, value, err).WithCmd(cmd)
	}
	return parsed, nil
}

func (p *builderFnValueParser[T]) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParseRef_[T](p, cmd, arg, value, source)
}

func (p *builderFnValueParser[T]) Parse(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	return BuilderTypedValueParserDefaultParse[T](p, cmd, arg, value)
}

func (p *builderFnValueParser[T]) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParse_[T](p, cmd, arg, value, source)
}

func (p *builderFnValueParser[T]) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderStringValueParser is clap::builder::StringValueParser:
// implementation for ValueParser::string.
//
// Useful for composing new TypedValueParsers.
type BuilderStringValueParser struct{}

// BuilderStringValueParserNew is StringValueParser::new: implementation for
// ValueParser::string.
func BuilderStringValueParserNew() BuilderStringValueParser { return BuilderStringValueParser{} }

func (p BuilderStringValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[string](p)
}

func (p BuilderStringValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return p.Parse(cmd, arg, value)
}

func (p BuilderStringValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParseRef_[string](p, cmd, arg, value, source)
}

func (p BuilderStringValueParser) Parse(cmd *Command, _ rs.Option[*Arg], value string) (string, error) {
	if !validUtf8(value) {
		return "", errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	return value, nil
}

func (p BuilderStringValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParse_[string](p, cmd, arg, value, source)
}

func (p BuilderStringValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderOsStringValueParser is clap::builder::OsStringValueParser:
// implementation for ValueParser::os_string.
//
// Useful for composing new TypedValueParsers.
type BuilderOsStringValueParser struct{}

// BuilderOsStringValueParserNew is OsStringValueParser::new.
func BuilderOsStringValueParserNew() BuilderOsStringValueParser { return BuilderOsStringValueParser{} }

func (p BuilderOsStringValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParser{inner: &builderTypedAdapter[string]{
		parser: p, id: utilAnyValueIdNamed[string]("std::ffi::os_str::OsString")}}
}

func (p BuilderOsStringValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return p.Parse(cmd, arg, value)
}

func (p BuilderOsStringValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParseRef_[string](p, cmd, arg, value, source)
}

func (p BuilderOsStringValueParser) Parse(_ *Command, _ rs.Option[*Arg], value string) (string, error) {
	return value, nil
}

func (p BuilderOsStringValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParse_[string](p, cmd, arg, value, source)
}

func (p BuilderOsStringValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderPathBufValueParser is clap::builder::PathBufValueParser:
// implementation for ValueParser::path_buf.
//
// Useful for composing new TypedValueParsers.
type BuilderPathBufValueParser struct{}

// BuilderPathBufValueParserNew is PathBufValueParser::new.
func BuilderPathBufValueParserNew() BuilderPathBufValueParser { return BuilderPathBufValueParser{} }

func (p BuilderPathBufValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParser{inner: &builderTypedAdapter[string]{
		parser: p, id: utilAnyValueIdNamed[string]("std::path::PathBuf")}}
}

func (p BuilderPathBufValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return p.Parse(cmd, arg, value)
}

func (p BuilderPathBufValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParseRef_[string](p, cmd, arg, value, source)
}

func (p BuilderPathBufValueParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	if len(value) == 0 {
		return "", errorEmptyValue(cmd, nil, argDisplayOr(arg, "..."))
	}
	return value, nil
}

func (p BuilderPathBufValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParse_[string](p, cmd, arg, value, source)
}

func (p BuilderPathBufValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderEnumValueParser is clap::builder::EnumValueParser: parse an
// ValueEnum value.
//
// See also:
//   - BuilderPossibleValuesParser
type BuilderEnumValueParser[E ValueEnum[E]] struct{}

// BuilderEnumValueParserNew is EnumValueParser::new: parse an ValueEnum.
func BuilderEnumValueParserNew[E ValueEnum[E]]() BuilderEnumValueParser[E] {
	return BuilderEnumValueParser[E]{}
}

func (p BuilderEnumValueParser[E]) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[E](p)
}

func (p BuilderEnumValueParser[E]) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (E, error) {
	var zero E
	ignoreCase := argIgnoreCase(arg)
	possibleVals := func() []string {
		var names []string
		for _, v := range zero.ValueVariants() {
			if pv, ok := v.ToPossibleValue().Get(); ok && !pv.IsHideSet() {
				names = append(names, pv.GetName())
			}
		}
		return names
	}

	if !validUtf8(value) {
		return zero, errorInvalidValue(cmd, toStringLossy(value), possibleVals(), argDisplayOr(arg, "..."))
	}
	for _, v := range zero.ValueVariants() {
		pv := v.ToPossibleValue().Expect("ValueEnum::value_variants contains only values with a corresponding ValueEnum::to_possible_value")
		if pv.Matches(value, ignoreCase) {
			return v, nil
		}
	}
	return zero, errorInvalidValue(cmd, value, possibleVals(), argDisplayOr(arg, "..."))
}

func (p BuilderEnumValueParser[E]) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (E, error) {
	return BuilderTypedValueParserDefaultParseRef_[E](p, cmd, arg, value, source)
}

func (p BuilderEnumValueParser[E]) Parse(cmd *Command, arg rs.Option[*Arg], value string) (E, error) {
	return BuilderTypedValueParserDefaultParse[E](p, cmd, arg, value)
}

func (p BuilderEnumValueParser[E]) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (E, error) {
	return BuilderTypedValueParserDefaultParse_[E](p, cmd, arg, value, source)
}

func (p BuilderEnumValueParser[E]) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	var zero E
	var values []BuilderPossibleValue
	for _, v := range zero.ValueVariants() {
		if pv, ok := v.ToPossibleValue().Get(); ok {
			values = append(values, pv)
		}
	}
	return rs.Some(values)
}

// BuilderPossibleValuesParser is clap::builder::PossibleValuesParser: verify
// the value is from an enumerated set of PossibleValue.
//
// See also:
//   - BuilderEnumValueParser for directly supporting ValueEnum types
//   - BuilderTypedValueParserMap for adapting values to a more specialized
//     type, like an external enums that can't implement ValueEnum
type BuilderPossibleValuesParser struct {
	values []BuilderPossibleValue
}

// BuilderPossibleValuesParserNew is PossibleValuesParser::new on names:
// verify the value is from an enumerated set of PossibleValue. It is also
// what `value_parser = ["a", "b"]` builds.
func BuilderPossibleValuesParserNew(values ...string) BuilderPossibleValuesParser {
	parser := BuilderPossibleValuesParser{values: make([]BuilderPossibleValue, len(values))}
	for i, value := range values {
		parser.values[i] = BuilderPossibleValueNew(value)
	}
	return parser
}

// BuilderPossibleValuesParserFrom is `impl From<I: IntoIterator<Item =
// impl Into<PossibleValue>>> for PossibleValuesParser`, for PossibleValues.
func BuilderPossibleValuesParserFrom(values ...BuilderPossibleValue) BuilderPossibleValuesParser {
	return BuilderPossibleValuesParser{values: append([]BuilderPossibleValue(nil), values...)}
}

func (p BuilderPossibleValuesParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[string](p)
}

func (p BuilderPossibleValuesParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return p.Parse(cmd, arg, value)
}

func (p BuilderPossibleValuesParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParseRef_[string](p, cmd, arg, value, source)
}

func (p BuilderPossibleValuesParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	if !validUtf8(value) {
		return "", errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}

	ignoreCase := argIgnoreCase(arg)
	for i := range p.values {
		if p.values[i].Matches(value, ignoreCase) {
			return value, nil
		}
	}
	var possibleVals []string
	for i := range p.values {
		if !p.values[i].IsHideSet() {
			possibleVals = append(possibleVals, p.values[i].GetName())
		}
	}

	return "", errorInvalidValue(cmd, value, possibleVals, argDisplayOr(arg, "..."))
}

func (p BuilderPossibleValuesParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParse_[string](p, cmd, arg, value, source)
}

func (p BuilderPossibleValuesParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return rs.Some(append([]BuilderPossibleValue(nil), p.values...))
}

// rangeBoundKind is std::ops::Bound's variant.
type rangeBoundKind uint8

const (
	rangeBound_Unbounded rangeBoundKind = iota
	rangeBound_Included
	rangeBound_Excluded
)

type rangeBound[T any] struct {
	kind  rangeBoundKind
	value T
}

// RangeBounds is a Rust range expression, as the methods that take
// `impl RangeBounds<T>` receive it. Build one with Range (`a..b`),
// RangeInclusive (`a..=b`), RangeFrom (`a..`), RangeTo (`..b`),
// RangeToInclusive (`..=b`) or RangeFull (`..`).
//
// It stands in for the std::ops range types until go/rs has them.
type RangeBounds[T any] struct {
	start rangeBound[T]
	end   rangeBound[T]
}

// Range is `start..end`.
func Range[T any](start, end T) RangeBounds[T] {
	return RangeBounds[T]{start: rangeBound[T]{kind: rangeBound_Included, value: start}, end: rangeBound[T]{kind: rangeBound_Excluded, value: end}}
}

// RangeInclusive is `start..=end`.
func RangeInclusive[T any](start, end T) RangeBounds[T] {
	return RangeBounds[T]{start: rangeBound[T]{kind: rangeBound_Included, value: start}, end: rangeBound[T]{kind: rangeBound_Included, value: end}}
}

// RangeFrom is `start..`.
func RangeFrom[T any](start T) RangeBounds[T] {
	return RangeBounds[T]{start: rangeBound[T]{kind: rangeBound_Included, value: start}}
}

// RangeTo is `..end`.
func RangeTo[T any](end T) RangeBounds[T] {
	return RangeBounds[T]{end: rangeBound[T]{kind: rangeBound_Excluded, value: end}}
}

// RangeToInclusive is `..=end`.
func RangeToInclusive[T any](end T) RangeBounds[T] {
	return RangeBounds[T]{end: rangeBound[T]{kind: rangeBound_Included, value: end}}
}

// RangeFull is `..`.
func RangeFull[T any]() RangeBounds[T] {
	return RangeBounds[T]{}
}

// builderInteger is the integer types a ranged parser can produce: the ones
// Rust has `TryFrom<i64>` or `TryFrom<u64>` for.
type builderInteger interface {
	~int8 | ~int16 | ~int32 | ~int64 | ~int | ~uint8 | ~uint16 | ~uint32 | ~uint64 | ~uint
}

// tryFromIntError is core::num::TryFromIntError: its Display text.
type tryFromIntError struct{}

func (tryFromIntError) Error() string { return "out of range integral type conversion attempted" }

// integerLimits returns the smallest and largest value of T, the largest as
// a uint64.
func integerLimits[T builderInteger]() (int64, uint64) {
	var zero T
	switch any(zero).(type) {
	case int8:
		return math.MinInt8, math.MaxInt8
	case int16:
		return math.MinInt16, math.MaxInt16
	case int32:
		return math.MinInt32, math.MaxInt32
	case int64:
		return math.MinInt64, math.MaxInt64
	case int:
		return math.MinInt, math.MaxInt
	case uint8:
		return 0, math.MaxUint8
	case uint16:
		return 0, math.MaxUint16
	case uint32:
		return 0, math.MaxUint32
	case uint64:
		return 0, math.MaxUint64
	case uint:
		return 0, math.MaxUint
	}
	// A defined type: read its limits from its kind.
	return integerLimitsOfKind(zero)
}

func i64Contains(bounds [2]rangeBound[int64], value int64) bool {
	switch bounds[0].kind {
	case rangeBound_Included:
		if !(bounds[0].value <= value) {
			return false
		}
	case rangeBound_Excluded:
		if !(bounds[0].value < value) {
			return false
		}
	}
	switch bounds[1].kind {
	case rangeBound_Included:
		if !(value <= bounds[1].value) {
			return false
		}
	case rangeBound_Excluded:
		if !(value < bounds[1].value) {
			return false
		}
	}
	return true
}

func u64Contains(bounds [2]rangeBound[uint64], value uint64) bool {
	switch bounds[0].kind {
	case rangeBound_Included:
		if !(bounds[0].value <= value) {
			return false
		}
	case rangeBound_Excluded:
		if !(bounds[0].value < value) {
			return false
		}
	}
	switch bounds[1].kind {
	case rangeBound_Included:
		if !(value <= bounds[1].value) {
			return false
		}
	case rangeBound_Excluded:
		if !(value < bounds[1].value) {
			return false
		}
	}
	return true
}

// boundsDebugI64 is the Debug form of `(Bound<i64>, Bound<i64>)`.
func boundDebug(kind rangeBoundKind, value string) string {
	switch kind {
	case rangeBound_Included:
		return "Included(" + value + ")"
	case rangeBound_Excluded:
		return "Excluded(" + value + ")"
	default:
		return "Unbounded"
	}
}

// BuilderRangedI64ValueParser is clap::builder::RangedI64ValueParser: parse
// number that fall within a range of values.
//
// NOTE: To capture negative values, you will also need to set
// Arg::allow_negative_numbers or Arg::allow_hyphen_values.
type BuilderRangedI64ValueParser[T builderInteger] struct {
	bounds [2]rangeBound[int64]
}

// BuilderRangedI64ValueParserNew is RangedI64ValueParser::new: select full
// range of `i64`.
func BuilderRangedI64ValueParserNew[T builderInteger]() *BuilderRangedI64ValueParser[T] {
	return &BuilderRangedI64ValueParser[T]{}
}

// Range is RangedI64ValueParser::range: narrow the supported range.
func (p *BuilderRangedI64ValueParser[T]) Range(r RangeBounds[int64]) *BuilderRangedI64ValueParser[T] {
	// Consideration: when the user does `value_parser!(u8).range()`
	// - Avoid programming mistakes by accidentally expanding the range
	// - Make it convenient to limit the range like with `..10`
	check := func(i int64, probe int64) {
		if debugAssertions && !i64Contains(p.bounds, probe) {
			rs.PanicAt(3, strconv.FormatInt(i, 10)+" must be in ("+
				boundDebug(p.bounds[0].kind, strconv.FormatInt(p.bounds[0].value, 10))+", "+
				boundDebug(p.bounds[1].kind, strconv.FormatInt(p.bounds[1].value, 10))+")")
		}
	}
	start := p.bounds[0]
	switch r.start.kind {
	case rangeBound_Included:
		check(r.start.value, r.start.value)
		start = r.start
	case rangeBound_Excluded:
		probe := r.start.value
		if probe < math.MaxInt64 {
			probe++
		}
		check(r.start.value, probe)
		start = r.start
	}
	end := p.bounds[1]
	switch r.end.kind {
	case rangeBound_Included:
		check(r.end.value, r.end.value)
		end = r.end
	case rangeBound_Excluded:
		probe := r.end.value
		if probe > math.MinInt64 {
			probe--
		}
		check(r.end.value, probe)
		end = r.end
	}
	return &BuilderRangedI64ValueParser[T]{bounds: [2]rangeBound[int64]{start, end}}
}

func (p *BuilderRangedI64ValueParser[T]) formatBounds() string {
	var result string
	switch p.bounds[0].kind {
	case rangeBound_Included:
		result = strconv.FormatInt(p.bounds[0].value, 10)
	case rangeBound_Excluded:
		i := p.bounds[0].value
		if i < math.MaxInt64 {
			i++
		}
		result = strconv.FormatInt(i, 10)
	default:
		result = strconv.FormatInt(math.MinInt64, 10)
	}
	result += ".."
	switch p.bounds[1].kind {
	case rangeBound_Included:
		result += "="
		result += strconv.FormatInt(p.bounds[1].value, 10)
	case rangeBound_Excluded:
		result += strconv.FormatInt(p.bounds[1].value, 10)
	default:
		result += strconv.FormatInt(math.MaxInt64, 10)
	}
	return result
}

func (p *BuilderRangedI64ValueParser[T]) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[T](p)
}

func (p *BuilderRangedI64ValueParser[T]) ParseRef(cmd *Command, arg rs.Option[*Arg], rawValue string) (T, error) {
	var zero T
	if !validUtf8(rawValue) {
		return zero, errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	value, err := parseInt(rawValue, 64)
	if err != nil {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(rawValue), err).WithCmd(cmd)
	}
	if !i64Contains(p.bounds, value) {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(rawValue),
			stringError(strconv.FormatInt(value, 10)+" is not in "+p.formatBounds())).WithCmd(cmd)
	}

	minValue, maxValue := integerLimits[T]()
	if value < minValue || (value > 0 && uint64(value) > maxValue) {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(rawValue), tryFromIntError{}).WithCmd(cmd)
	}

	return T(value), nil
}

func (p *BuilderRangedI64ValueParser[T]) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParseRef_[T](p, cmd, arg, value, source)
}

func (p *BuilderRangedI64ValueParser[T]) Parse(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	return BuilderTypedValueParserDefaultParse[T](p, cmd, arg, value)
}

func (p *BuilderRangedI64ValueParser[T]) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParse_[T](p, cmd, arg, value, source)
}

func (p *BuilderRangedI64ValueParser[T]) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderRangedI64ValueParserFromRange is `impl<T, B: RangeBounds<i64>>
// From<B> for RangedI64ValueParser<T>`.
func BuilderRangedI64ValueParserFromRange[T builderInteger](r RangeBounds[int64]) *BuilderRangedI64ValueParser[T] {
	return &BuilderRangedI64ValueParser[T]{bounds: [2]rangeBound[int64]{r.start, r.end}}
}

// BuilderRangedU64ValueParser is clap::builder::RangedU64ValueParser: parse
// number that fall within a range of values.
type BuilderRangedU64ValueParser[T builderInteger] struct {
	bounds [2]rangeBound[uint64]
}

// BuilderRangedU64ValueParserNew is RangedU64ValueParser::new: select full
// range of `u64`.
func BuilderRangedU64ValueParserNew[T builderInteger]() *BuilderRangedU64ValueParser[T] {
	return &BuilderRangedU64ValueParser[T]{}
}

// Range is RangedU64ValueParser::range: narrow the supported range.
func (p *BuilderRangedU64ValueParser[T]) Range(r RangeBounds[uint64]) *BuilderRangedU64ValueParser[T] {
	// Consideration: when the user does `value_parser!(u8).range()`
	// - Avoid programming mistakes by accidentally expanding the range
	// - Make it convenient to limit the range like with `..10`
	check := func(i uint64, probe uint64) {
		if debugAssertions && !u64Contains(p.bounds, probe) {
			rs.PanicAt(3, strconv.FormatUint(i, 10)+" must be in ("+
				boundDebug(p.bounds[0].kind, strconv.FormatUint(p.bounds[0].value, 10))+", "+
				boundDebug(p.bounds[1].kind, strconv.FormatUint(p.bounds[1].value, 10))+")")
		}
	}
	start := p.bounds[0]
	switch r.start.kind {
	case rangeBound_Included:
		check(r.start.value, r.start.value)
		start = r.start
	case rangeBound_Excluded:
		probe := r.start.value
		if probe < math.MaxUint64 {
			probe++
		}
		check(r.start.value, probe)
		start = r.start
	}
	end := p.bounds[1]
	switch r.end.kind {
	case rangeBound_Included:
		check(r.end.value, r.end.value)
		end = r.end
	case rangeBound_Excluded:
		probe := r.end.value
		if probe > 0 {
			probe--
		}
		check(r.end.value, probe)
		end = r.end
	}
	return &BuilderRangedU64ValueParser[T]{bounds: [2]rangeBound[uint64]{start, end}}
}

func (p *BuilderRangedU64ValueParser[T]) formatBounds() string {
	var result string
	switch p.bounds[0].kind {
	case rangeBound_Included:
		result = strconv.FormatUint(p.bounds[0].value, 10)
	case rangeBound_Excluded:
		i := p.bounds[0].value
		if i < math.MaxUint64 {
			i++
		}
		result = strconv.FormatUint(i, 10)
	default:
		result = "0"
	}
	result += ".."
	switch p.bounds[1].kind {
	case rangeBound_Included:
		result += "="
		result += strconv.FormatUint(p.bounds[1].value, 10)
	case rangeBound_Excluded:
		result += strconv.FormatUint(p.bounds[1].value, 10)
	default:
		result += strconv.FormatUint(math.MaxUint64, 10)
	}
	return result
}

func (p *BuilderRangedU64ValueParser[T]) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[T](p)
}

func (p *BuilderRangedU64ValueParser[T]) ParseRef(cmd *Command, arg rs.Option[*Arg], rawValue string) (T, error) {
	var zero T
	if !validUtf8(rawValue) {
		return zero, errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	value, err := parseUint(rawValue, 64)
	if err != nil {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(rawValue), err).WithCmd(cmd)
	}
	if !u64Contains(p.bounds, value) {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(rawValue),
			stringError(strconv.FormatUint(value, 10)+" is not in "+p.formatBounds())).WithCmd(cmd)
	}

	_, maxValue := integerLimits[T]()
	if value > maxValue {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(rawValue), tryFromIntError{}).WithCmd(cmd)
	}

	return T(value), nil
}

func (p *BuilderRangedU64ValueParser[T]) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParseRef_[T](p, cmd, arg, value, source)
}

func (p *BuilderRangedU64ValueParser[T]) Parse(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	return BuilderTypedValueParserDefaultParse[T](p, cmd, arg, value)
}

func (p *BuilderRangedU64ValueParser[T]) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParse_[T](p, cmd, arg, value, source)
}

func (p *BuilderRangedU64ValueParser[T]) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderRangedU64ValueParserFromRange is `impl<T, B: RangeBounds<u64>>
// From<B> for RangedU64ValueParser<T>`.
func BuilderRangedU64ValueParserFromRange[T builderInteger](r RangeBounds[uint64]) *BuilderRangedU64ValueParser[T] {
	return &BuilderRangedU64ValueParser[T]{bounds: [2]rangeBound[uint64]{r.start, r.end}}
}

// BuilderBoolValueParser is clap::builder::BoolValueParser: implementation
// for ValueParser::bool.
//
// Useful for composing new TypedValueParsers.
type BuilderBoolValueParser struct{}

// BuilderBoolValueParserNew is BoolValueParser::new.
func BuilderBoolValueParserNew() BuilderBoolValueParser { return BuilderBoolValueParser{} }

func builderBoolPossibleValues() []BuilderPossibleValue {
	return []BuilderPossibleValue{BuilderPossibleValueNew("true"), BuilderPossibleValueNew("false")}
}

func (p BuilderBoolValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[bool](p)
}

func (p BuilderBoolValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (bool, error) {
	if value == "true" {
		return true, nil
	} else if value == "false" {
		return false, nil
	}
	// Intentionally showing hidden as we hide all of them
	possibleVals := []string{"true", "false"}

	return false, errorInvalidValue(cmd, toStringLossy(value), possibleVals, argDisplayOr(arg, "..."))
}

func (p BuilderBoolValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (bool, error) {
	return BuilderTypedValueParserDefaultParseRef_[bool](p, cmd, arg, value, source)
}

func (p BuilderBoolValueParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (bool, error) {
	return BuilderTypedValueParserDefaultParse[bool](p, cmd, arg, value)
}

func (p BuilderBoolValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (bool, error) {
	return BuilderTypedValueParserDefaultParse_[bool](p, cmd, arg, value, source)
}

func (p BuilderBoolValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return rs.Some(builderBoolPossibleValues())
}

func builderBoolishPossibleValues() []BuilderPossibleValue {
	var values []BuilderPossibleValue
	for _, l := range utilTrueLiterals {
		values = append(values, BuilderPossibleValueNew(l).Hide(l != "true" && l != "false"))
	}
	for _, l := range utilFalseLiterals {
		values = append(values, BuilderPossibleValueNew(l).Hide(l != "true" && l != "false"))
	}
	return values
}

// BuilderFalseyValueParser is clap::builder::FalseyValueParser: parse
// false-like string values, everything else is `true`.
//
// See also:
//   - ValueParser::bool for assuming non-false is true
//   - BuilderBoolishValueParser for different human readable bool
//     representations
type BuilderFalseyValueParser struct{}

// BuilderFalseyValueParserNew is FalseyValueParser::new: parse false-like
// string values, everything else is `true`.
func BuilderFalseyValueParserNew() BuilderFalseyValueParser { return BuilderFalseyValueParser{} }

func (p BuilderFalseyValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[bool](p)
}

func (p BuilderFalseyValueParser) ParseRef(cmd *Command, _ rs.Option[*Arg], value string) (bool, error) {
	if !validUtf8(value) {
		return false, errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	if len(value) == 0 {
		return false, nil
	}
	parsed, ok := utilStrToBool(value)
	if !ok {
		return true, nil
	}
	return parsed, nil
}

func (p BuilderFalseyValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (bool, error) {
	return BuilderTypedValueParserDefaultParseRef_[bool](p, cmd, arg, value, source)
}

func (p BuilderFalseyValueParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (bool, error) {
	return BuilderTypedValueParserDefaultParse[bool](p, cmd, arg, value)
}

func (p BuilderFalseyValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (bool, error) {
	return BuilderTypedValueParserDefaultParse_[bool](p, cmd, arg, value, source)
}

func (p BuilderFalseyValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return rs.Some(builderBoolishPossibleValues())
}

// BuilderBoolishValueParser is clap::builder::BoolishValueParser: parse
// bool-like string values.
//
// See also:
//   - ValueParser::bool for different human readable bool representations
//   - BuilderFalseyValueParser for assuming non-false is true
type BuilderBoolishValueParser struct{}

// BuilderBoolishValueParserNew is BoolishValueParser::new: parse bool-like
// string values.
func BuilderBoolishValueParserNew() BuilderBoolishValueParser { return BuilderBoolishValueParser{} }

func (p BuilderBoolishValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[bool](p)
}

func (p BuilderBoolishValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (bool, error) {
	if !validUtf8(value) {
		return false, errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	parsed, ok := utilStrToBool(value)
	if !ok {
		argText := argDisplayOr(arg, "...")
		return false, errorValueValidation(argText, value, stringError("value was not a boolean")).WithCmd(cmd)
	}
	return parsed, nil
}

func (p BuilderBoolishValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (bool, error) {
	return BuilderTypedValueParserDefaultParseRef_[bool](p, cmd, arg, value, source)
}

func (p BuilderBoolishValueParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (bool, error) {
	return BuilderTypedValueParserDefaultParse[bool](p, cmd, arg, value)
}

func (p BuilderBoolishValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (bool, error) {
	return BuilderTypedValueParserDefaultParse_[bool](p, cmd, arg, value, source)
}

func (p BuilderBoolishValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return rs.Some(builderBoolishPossibleValues())
}

// BuilderNonEmptyStringValueParser is
// clap::builder::NonEmptyStringValueParser: parse non-empty string values.
//
// See also:
//   - ValueParser::string
type BuilderNonEmptyStringValueParser struct{}

// BuilderNonEmptyStringValueParserNew is NonEmptyStringValueParser::new:
// parse non-empty string values.
func BuilderNonEmptyStringValueParserNew() BuilderNonEmptyStringValueParser {
	return BuilderNonEmptyStringValueParser{}
}

func (p BuilderNonEmptyStringValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[string](p)
}

func (p BuilderNonEmptyStringValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	if len(value) == 0 {
		return "", errorEmptyValue(cmd, nil, argDisplayOr(arg, "..."))
	}
	if !validUtf8(value) {
		return "", errorInvalidUtf8(cmd, outputUsageNew(cmd).createUsageWithTitle(nil))
	}
	return value, nil
}

func (p BuilderNonEmptyStringValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParseRef_[string](p, cmd, arg, value, source)
}

func (p BuilderNonEmptyStringValueParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return BuilderTypedValueParserDefaultParse[string](p, cmd, arg, value)
}

func (p BuilderNonEmptyStringValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParse_[string](p, cmd, arg, value, source)
}

func (p BuilderNonEmptyStringValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// BuilderMapValueParser is clap::builder::MapValueParser: adapt a
// TypedValueParser from one value to another.
//
// See BuilderTypedValueParserMap.
type BuilderMapValueParser[B, T any] struct {
	parser BuilderTypedValueParser[B]
	f      func(B) T
}

func (p *BuilderMapValueParser[B, T]) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[T](p)
}

func (p *BuilderMapValueParser[B, T]) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	var zero T
	parsed, err := p.parser.ParseRef(cmd, arg, value)
	if err != nil {
		return zero, err
	}
	return p.f(parsed), nil
}

func (p *BuilderMapValueParser[B, T]) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParseRef_[T](p, cmd, arg, value, source)
}

func (p *BuilderMapValueParser[B, T]) Parse(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	var zero T
	parsed, err := p.parser.Parse(cmd, arg, value)
	if err != nil {
		return zero, err
	}
	return p.f(parsed), nil
}

func (p *BuilderMapValueParser[B, T]) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParse_[T](p, cmd, arg, value, source)
}

func (p *BuilderMapValueParser[B, T]) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return p.parser.PossibleValues()
}

// BuilderTryMapValueParser is clap::builder::TryMapValueParser: adapt a
// TypedValueParser from one value to another.
//
// See BuilderTypedValueParserTryMap.
type BuilderTryMapValueParser[B, T any] struct {
	parser BuilderTypedValueParser[B]
	f      func(B) (T, error)
}

func (p *BuilderTryMapValueParser[B, T]) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[T](p)
}

func (p *BuilderTryMapValueParser[B, T]) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	var zero T
	midValue, err := p.parser.ParseRef(cmd, arg, value)
	if err != nil {
		return zero, err
	}
	parsed, mapErr := p.f(midValue)
	if mapErr != nil {
		argText := argDisplayOr(arg, "...")
		return zero, errorValueValidation(argText, toStringLossy(value), mapErr).WithCmd(cmd)
	}
	return parsed, nil
}

func (p *BuilderTryMapValueParser[B, T]) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParseRef_[T](p, cmd, arg, value, source)
}

func (p *BuilderTryMapValueParser[B, T]) Parse(cmd *Command, arg rs.Option[*Arg], value string) (T, error) {
	return BuilderTypedValueParserDefaultParse[T](p, cmd, arg, value)
}

func (p *BuilderTryMapValueParser[B, T]) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (T, error) {
	return BuilderTypedValueParserDefaultParse_[T](p, cmd, arg, value, source)
}

func (p *BuilderTryMapValueParser[B, T]) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return p.parser.PossibleValues()
}

// BuilderUnknownArgumentValueParser is
// clap::builder::UnknownArgumentValueParser: when encountered, report
// ErrorKind::UnknownArgument.
//
// Useful to help users migrate, either from old versions or similar tools.
type BuilderUnknownArgumentValueParser struct {
	arg         rs.Option[string]
	suggestions []BuilderStyledStr
}

// BuilderUnknownArgumentValueParserSuggestArg is
// UnknownArgumentValueParser::suggest_arg: suggest an alternative argument.
func BuilderUnknownArgumentValueParserSuggestArg(arg string) BuilderUnknownArgumentValueParser {
	return BuilderUnknownArgumentValueParser{arg: rs.Some(arg)}
}

// BuilderUnknownArgumentValueParserSuggest is
// UnknownArgumentValueParser::suggest: provide a general suggestion.
func BuilderUnknownArgumentValueParserSuggest(text string) BuilderUnknownArgumentValueParser {
	return BuilderUnknownArgumentValueParser{suggestions: []BuilderStyledStr{BuilderStyledStr(text)}}
}

// AndSuggest is UnknownArgumentValueParser::and_suggest: extend the
// suggestions.
func (p BuilderUnknownArgumentValueParser) AndSuggest(text string) BuilderUnknownArgumentValueParser {
	p.suggestions = append(append([]BuilderStyledStr(nil), p.suggestions...), BuilderStyledStr(text))
	return p
}

func (p BuilderUnknownArgumentValueParser) IntoValueParser() BuilderValueParser {
	return BuilderValueParserNew[string](p)
}

func (p BuilderUnknownArgumentValueParser) ParseRef(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return p.ParseRef_(cmd, arg, value, ParserValueSource_CommandLine)
}

func (p BuilderUnknownArgumentValueParser) ParseRef_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	switch source {
	case ParserValueSource_DefaultValue:
		return BuilderStringValueParserNew().ParseRef_(cmd, arg, value, source)
	case ParserValueSource_EnvVariable, ParserValueSource_CommandLine:
		argText := argDisplayOr(arg, "..")
		var didYouMean rs.Option[errorDidYouMean]
		if suggested, ok := p.arg.Get(); ok {
			didYouMean = rs.Some(errorDidYouMean{flag: suggested})
		}
		err := errorUnknownArgument(cmd, argText, didYouMean, false, outputUsageNew(cmd).createUsageWithTitle(nil))
		if debugAssertions && err.Get(ErrorContextKind_Suggested).IsSome() {
			rs.PanicAt(1, "Assuming `Error::unknown_argument` doesn't apply any `Suggested` so we can without caution")
		}
		err = err.insertContextUnchecked(ErrorContextKind_Suggested,
			&ErrorContextValue_StyledStrs{F0: append([]BuilderStyledStr(nil), p.suggestions...)})
		return "", err
	default:
		panic(rs.Unreachable())
	}
}

func (p BuilderUnknownArgumentValueParser) Parse(cmd *Command, arg rs.Option[*Arg], value string) (string, error) {
	return BuilderTypedValueParserDefaultParse[string](p, cmd, arg, value)
}

func (p BuilderUnknownArgumentValueParser) Parse_(cmd *Command, arg rs.Option[*Arg], value string, source ParserValueSource) (string, error) {
	return BuilderTypedValueParserDefaultParse_[string](p, cmd, arg, value, source)
}

func (p BuilderUnknownArgumentValueParser) PossibleValues() rs.Option[[]BuilderPossibleValue] {
	return BuilderTypedValueParserDefaultPossibleValues()
}

// The functions below are Rust's `value_parser!(T)`: select a ValueParser
// implementation from the intended type.
//
// Supported types
//   - ValueParserFactory types, including
//     - Native types: `bool`, `String`, `OsString`, `PathBuf`
//     - Ranged numeric types: `u8`, `i8`, `u16`, `i16`, `u32`, `i32`, `u64`,
//       `i64`
//   - ValueEnum types
//   - From<OsString> types and From<&OsStr> types
//   - From<String> types and From<&str> types
//   - FromStr types, including usize, isize
//
// Rust picks among them from the type's traits. Go has to be told: the
// function's name carries the Rust type, or the trait the macro would have
// found.

// ValueParserString is `value_parser!(String)`.
func ValueParserString() BuilderValueParser { return BuilderValueParserString() }

// ValueParserOsString is `value_parser!(OsString)`.
func ValueParserOsString() BuilderValueParser { return BuilderValueParserOsString() }

// ValueParserPathBuf is `value_parser!(PathBuf)`.
func ValueParserPathBuf() BuilderValueParser { return BuilderValueParserPathBuf() }

// ValueParserBool is `value_parser!(bool)`.
func ValueParserBool() BuilderValueParser { return BuilderValueParserBool() }

// ValueParserU8 is `value_parser!(u8)`.
func ValueParserU8() *BuilderRangedI64ValueParser[uint8] {
	return BuilderRangedI64ValueParserNew[uint8]().Range(RangeInclusive[int64](0, math.MaxUint8))
}

// ValueParserI8 is `value_parser!(i8)`.
func ValueParserI8() *BuilderRangedI64ValueParser[int8] {
	return BuilderRangedI64ValueParserNew[int8]().Range(RangeInclusive[int64](math.MinInt8, math.MaxInt8))
}

// ValueParserU16 is `value_parser!(u16)`.
func ValueParserU16() *BuilderRangedI64ValueParser[uint16] {
	return BuilderRangedI64ValueParserNew[uint16]().Range(RangeInclusive[int64](0, math.MaxUint16))
}

// ValueParserI16 is `value_parser!(i16)`.
func ValueParserI16() *BuilderRangedI64ValueParser[int16] {
	return BuilderRangedI64ValueParserNew[int16]().Range(RangeInclusive[int64](math.MinInt16, math.MaxInt16))
}

// ValueParserU32 is `value_parser!(u32)`.
func ValueParserU32() *BuilderRangedI64ValueParser[uint32] {
	return BuilderRangedI64ValueParserNew[uint32]().Range(RangeInclusive[int64](0, math.MaxUint32))
}

// ValueParserI32 is `value_parser!(i32)`.
func ValueParserI32() *BuilderRangedI64ValueParser[int32] {
	return BuilderRangedI64ValueParserNew[int32]().Range(RangeInclusive[int64](math.MinInt32, math.MaxInt32))
}

// ValueParserU64 is `value_parser!(u64)`.
func ValueParserU64() *BuilderRangedU64ValueParser[uint64] {
	return BuilderRangedU64ValueParserNew[uint64]()
}

// ValueParserI64 is `value_parser!(i64)`.
func ValueParserI64() *BuilderRangedI64ValueParser[int64] {
	return BuilderRangedI64ValueParserNew[int64]()
}

// ValueParserUsize is `value_parser!(usize)`: usize has no
// ValueParserFactory, so the macro falls back to its FromStr.
//
// A usize is an int in the port, so a value above i64::MAX, which Rust
// accepts on a 64-bit target, is refused here as too large.
func ValueParserUsize() BuilderValueParser {
	return valueParserFromStrNamed("usize", func(s string) (int, error) {
		value, err := parseUint(s, 64)
		if err != nil {
			return 0, err
		}
		if value > math.MaxInt {
			return 0, errIntPosOverflow
		}
		return int(value), nil
	})
}

// ValueParserIsize is `value_parser!(isize)`, through isize's FromStr.
func ValueParserIsize() BuilderValueParser {
	return valueParserFromStrNamed("isize", func(s string) (int, error) {
		value, err := parseInt(s, 64)
		return int(value), err
	})
}

// ValueParserF64 is `value_parser!(f64)`, through f64's FromStr.
func ValueParserF64() BuilderValueParser {
	return valueParserFromStrNamed("f64", func(s string) (float64, error) {
		return parseFloat(s, 64)
	})
}

// ValueParserF32 is `value_parser!(f32)`, through f32's FromStr.
func ValueParserF32() BuilderValueParser {
	return valueParserFromStrNamed("f32", func(s string) (float32, error) {
		value, err := parseFloat(s, 32)
		return float32(value), err
	})
}

// ValueParserEnum is `value_parser!(E)` for a type that implements
// ValueEnum.
func ValueParserEnum[E ValueEnum[E]]() BuilderEnumValueParser[E] {
	return BuilderEnumValueParserNew[E]()
}

// ValueParserFromStr is `value_parser!(T)` for a type that implements
// FromStr and none of the traits the macro prefers: fromStr is the type's
// `TFromStr` function.
func ValueParserFromStr[T any](fromStr func(string) (T, error)) BuilderValueParser {
	return BuilderValueParserFromFn(fromStr)
}

func valueParserFromStrNamed[T any](rustName string, fromStr func(string) (T, error)) BuilderValueParser {
	return BuilderValueParser{inner: &builderTypedAdapter[T]{
		parser: &builderFnValueParser[T]{f: fromStr}, id: utilAnyValueIdNamed[T](rustName)}}
}

// ValueParserFromString is `value_parser!(T)` for a type that implements
// From<String> or From<&str>: from is that conversion.
func ValueParserFromString[T any](from func(string) T) BuilderValueParser {
	return BuilderTypedValueParserMap[string, T](BuilderStringValueParserNew(), from).IntoValueParser()
}

// ValueParserFromOsString is `value_parser!(T)` for a type that implements
// From<OsString> or From<&OsStr>: from is that conversion.
func ValueParserFromOsString[T any](from func(string) T) BuilderValueParser {
	return BuilderTypedValueParserMap[string, T](BuilderOsStringValueParserNew(), from).IntoValueParser()
}
