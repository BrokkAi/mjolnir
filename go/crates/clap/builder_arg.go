// Port of clap_builder 4.6.0 src/builder/arg.rs

package clap

import (
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// argRequire is one `(ArgPredicate, Id)` of Arg::requires.
type argRequire struct {
	predicate BuilderArgPredicate
	id        Id
}

// argIf is one `(Id, OsStr)` of Arg::r_ifs.
type argIf struct {
	id  Id
	val string
}

// argAlias is one `(Str, bool)` of Arg::aliases: (name, visible).
type argAlias struct {
	name    string
	visible bool
}

// argShortAlias is one `(char, bool)` of Arg::short_aliases: (name, visible).
type argShortAlias struct {
	name    rune
	visible bool
}

// argDefaultIf is one `(Id, ArgPredicate, Option<Vec<OsStr>>)` of
// Arg::default_vals_ifs.
type argDefaultIf struct {
	id        Id
	predicate BuilderArgPredicate
	defaults  rs.Option[[]string]
}

// argEnv is the `(OsStr, Option<OsString>)` of Arg::env: the variable's name
// and the value it had when the argument was defined.
type argEnv struct {
	name  string
	value rs.Option[string]
}

// Arg is clap::Arg: the abstract representation of a command line argument.
// Used to set all the options and relationships that define a valid
// argument for the program.
//
// There are two methods for constructing Args, using the builder pattern
// and setting options manually, or using a usage string which is far less
// verbose but has fewer options. You can also use a combination of the two
// methods to achieve the best of both worlds.
//
//   - Basic API
//   - Value Handling
//   - Help
//   - Advanced Argument Relations
//   - Reflection
//
// The builder methods change the argument and return it, so calls chain as
// they do in Rust. Where Rust passes `&arg` (which clones), pass
// arg.Clone().
//
// A method whose Rust parameter is `impl IntoResettable<T>` takes the value;
// passing `None` is the method of the same name with `Reset` appended.
type Arg struct {
	id                 Id
	help               rs.Option[BuilderStyledStr]
	longHelp           rs.Option[BuilderStyledStr]
	action             rs.Option[ArgAction]
	valueParser        rs.Option[BuilderValueParser]
	blacklist          []Id
	settings           argFlags
	overrides          []Id
	groups             []Id
	requires           []argRequire
	rIfs               []argIf
	rIfsAll            []argIf
	rUnless            []Id
	rUnlessAll         []Id
	short              rs.Option[rune]
	long               rs.Option[string]
	aliases            []argAlias      // (name, visible)
	shortAliases       []argShortAlias // (name, visible)
	dispOrd            rs.Option[int]
	valNames           []string
	numVals            rs.Option[BuilderValueRange]
	valDelim           rs.Option[rune]
	defaultVals        []string
	defaultValsIfs     []argDefaultIf
	defaultMissingVals []string
	env                rs.Option[argEnv]
	terminator         rs.Option[string]
	index              rs.Option[int]
	helpHeading        rs.Option[rs.Option[string]]
	// ext holds only the ValueHint: the crate's `unstable-ext` feature is
	// off.
	valueHint rs.Option[ValueHint]
}

// # Basic API

// ArgNew is Arg::new: create a new Arg with a unique name.
//
// The name is used to check whether or not the argument was used at
// runtime, get values, set relationships with other args, etc..
//
// By default, an Arg is
//   - Positional, see Arg::short or Arg::long to turn it into an option
//   - Accept a single value, see Arg::action to override this
//
// NOTE: In the case of arguments that take values (i.e.
// Arg::action(ArgAction::Set)) and positional arguments (i.e. those without
// a preceding `-` or `--`) the name will also be displayed when the user
// prints the usage/help information of the program.
func ArgNew(id string) *Arg {
	return (&Arg{}).Id(id)
}

// Id is Arg::id: set the identifier used for referencing this argument in
// the clap API.
//
// See Arg::new for more details.
func (a *Arg) Id(id string) *Arg {
	a.id = Id(id)
	return a
}

// Short is Arg::short: sets the short version of the argument without the
// preceding `-`.
//
// By default `V` and `h` are used by the auto-generated `version` and `help`
// arguments, respectively. You will need to disable the auto-generated flags
// (disable_help_flag, disable_version_flag) to use these.
func (a *Arg) Short(s rune) *Arg {
	if debugAssertions && s == '-' {
		rs.PanicAt(1, "short option name cannot be `-`")
	}
	a.short = rs.Some(s)
	return a
}

// ShortReset is Arg::short(None).
func (a *Arg) ShortReset() *Arg {
	a.short = rs.None[rune]()
	return a
}

// Long is Arg::long: sets the long version of the argument without the
// preceding `--`.
//
// By default `version` and `help` are used by the auto-generated `version`
// and `help` arguments, respectively. You may use the word `version` or
// `help` for the long form of your own arguments, in which case `clap`
// simply will not assign those to the auto-generated `version` or `help`
// arguments.
//
// NOTE: Any leading `-` characters will be stripped
func (a *Arg) Long(l string) *Arg {
	a.long = rs.Some(l)
	return a
}

// LongReset is Arg::long(None).
func (a *Arg) LongReset() *Arg {
	a.long = rs.None[string]()
	return a
}

// Alias is Arg::alias: add an alias, which functions as a hidden long flag.
//
// This is more efficient, and easier than creating multiple hidden
// arguments as one only needs to check for the existence of this command,
// and not all variants.
func (a *Arg) Alias(name string) *Arg {
	a.aliases = append(a.aliases, argAlias{name: name, visible: false})
	return a
}

// AliasReset is Arg::alias(None): removes the aliases.
func (a *Arg) AliasReset() *Arg {
	a.aliases = nil
	return a
}

// ShortAlias is Arg::short_alias: add an alias, which functions as a hidden
// short flag.
//
// This is more efficient, and easier than creating multiple hidden
// arguments as one only needs to check for the existence of this command,
// and not all variants.
func (a *Arg) ShortAlias(name rune) *Arg {
	if debugAssertions && name == '-' {
		rs.PanicAt(1, "short alias name cannot be `-`")
	}
	a.shortAliases = append(a.shortAliases, argShortAlias{name: name, visible: false})
	return a
}

// ShortAliasReset is Arg::short_alias(None): removes the short aliases.
func (a *Arg) ShortAliasReset() *Arg {
	a.shortAliases = nil
	return a
}

// Aliases is Arg::aliases: add aliases, which function as hidden long
// flags.
//
// This is more efficient, and easier than creating multiple hidden
// subcommands as one only needs to check for the existence of this command,
// and not all variants.
func (a *Arg) Aliases(names ...string) *Arg {
	for _, name := range names {
		a.aliases = append(a.aliases, argAlias{name: name, visible: false})
	}
	return a
}

// ShortAliases is Arg::short_aliases: add aliases, which functions as a
// hidden short flag.
//
// This is more efficient, and easier than creating multiple hidden
// subcommands as one only needs to check for the existence of this command,
// and not all variants.
func (a *Arg) ShortAliases(names ...rune) *Arg {
	for _, s := range names {
		if debugAssertions && s == '-' {
			rs.PanicAt(1, "short alias name cannot be `-`")
		}
		a.shortAliases = append(a.shortAliases, argShortAlias{name: s, visible: false})
	}
	return a
}

// VisibleAlias is Arg::visible_alias: add an alias, which functions as a
// visible long flag.
//
// Like Arg::alias, except that they are visible inside the help message.
func (a *Arg) VisibleAlias(name string) *Arg {
	a.aliases = append(a.aliases, argAlias{name: name, visible: true})
	return a
}

// VisibleAliasReset is Arg::visible_alias(None): removes the aliases.
func (a *Arg) VisibleAliasReset() *Arg {
	a.aliases = nil
	return a
}

// VisibleShortAlias is Arg::visible_short_alias: add an alias, which
// functions as a visible short flag.
//
// Like Arg::short_alias, except that they are visible inside the help
// message.
func (a *Arg) VisibleShortAlias(name rune) *Arg {
	if debugAssertions && name == '-' {
		rs.PanicAt(1, "short alias name cannot be `-`")
	}
	a.shortAliases = append(a.shortAliases, argShortAlias{name: name, visible: true})
	return a
}

// VisibleShortAliasReset is Arg::visible_short_alias(None): removes the
// short aliases.
func (a *Arg) VisibleShortAliasReset() *Arg {
	a.shortAliases = nil
	return a
}

// VisibleAliases is Arg::visible_aliases: add aliases, which function as
// visible long flags.
//
// Like Arg::aliases, except that they are visible inside the help message.
func (a *Arg) VisibleAliases(names ...string) *Arg {
	for _, name := range names {
		a.aliases = append(a.aliases, argAlias{name: name, visible: true})
	}
	return a
}

// VisibleShortAliases is Arg::visible_short_aliases: add aliases, which
// function as visible short flags.
//
// Like Arg::short_aliases, except that they are visible inside the help
// message.
func (a *Arg) VisibleShortAliases(names ...rune) *Arg {
	for _, n := range names {
		if debugAssertions && n == '-' {
			rs.PanicAt(1, "short alias name cannot be `-`")
		}
		a.shortAliases = append(a.shortAliases, argShortAlias{name: n, visible: true})
	}
	return a
}

// Index is Arg::index: specifies the index of a positional argument
// **starting at** 1.
//
// NOTE: The index refers to position according to other positional
// argument. It does not define position in the argument list as a whole.
//
// NOTE: You can optionally leave off the `index` method, and the index will
// be assigned in order of evaluation. Utilizing the `index` method allows
// for setting indexes out of order
//
// NOTE: This is only meant to be used for positional arguments and shouldn't
// to be used with Arg::short or Arg::long.
//
// NOTE: When utilized with [Arg::num_args(1..)], only the **last**
// positional argument may be defined as having a variable number of
// arguments (i.e. with the highest index)
//
// Panics: Command will panic! if indexes are skipped (such as defining
// `index(1)` and `index(3)` but not `index(2)`, or a positional argument is
// defined as multiple and is not the highest index (debug builds)
func (a *Arg) Index(idx int) *Arg {
	a.index = rs.Some(idx)
	return a
}

// IndexReset is Arg::index(None).
func (a *Arg) IndexReset() *Arg {
	a.index = rs.None[int]()
	return a
}

// TrailingVarArg is Arg::trailing_var_arg: this is a "var arg" and
// everything that follows should be captured by it, as if the user had used
// a `--`.
//
// NOTE: To start the trailing "var arg" on unknown flags (and not just a
// positional value), set allow_hyphen_values. Either way, users still have
// the option to explicitly escape ambiguous arguments with `--`.
//
// NOTE: Arg::value_delimiter still applies if set.
//
// NOTE: Setting this requires Arg::num_args(..).
func (a *Arg) TrailingVarArg(yes bool) *Arg {
	return a.setSetting(argSettings_TrailingVarArg, yes)
}

// Last is Arg::last: this arg is the last, or final, positional argument
// (i.e. has the highest index) and is *only* able to be accessed via the
// `--` syntax (i.e. `$ prog args -- last_arg`).
//
// Even, if no other arguments are left to parse, if the user omits the
// `--` syntax they will receive an UnknownArgument error. Setting an
// argument to `.last(true)` also allows one to access this arg early using
// the `--` syntax. Accessing an arg early, even with the `--` syntax is
// otherwise not possible.
//
// NOTE: This will change the usage string to look like `$ prog [OPTIONS]
// [-- <ARG>]` if `ARG` is marked as `.last(true)`.
//
// NOTE: This setting will imply
// crate::Command::dont_collapse_args_in_usage because failing to use the
// `--` syntax results in an error.
//
// NOTE: This setting only applies to positional arguments, and has no
// effect on OPTIONS
//
// NOTE: Setting this requires taking values
//
// CAUTION: Using this setting *and* having child subcommands is not
// recommended with the exception of *also* using
// crate::Command::args_conflicts_with_subcommands (or
// crate::Command::subcommand_negates_reqs if the argument marked `Last` is
// also marked Arg::required)
func (a *Arg) Last(yes bool) *Arg {
	return a.setSetting(argSettings_Last, yes)
}

// Required is Arg::required: specifies that the argument must be present.
//
// Required by default means it is required, when no other conflicting rules
// or overrides have been evaluated. Conflicting rules take precedence over
// being required.
//
// Pro tip: Flags (i.e. not positional, or arguments that take values)
// shouldn't be required by default. This is because if a flag were to be
// required, it should simply be implied. No additional information is
// required from user. Flags by their very nature are simply boolean on/off
// switches. The only time a user *should* be required to use a flag is if
// the operation is destructive in nature, and the user is essentially
// proving to you, "Yes, I know what I'm doing."
func (a *Arg) Required(yes bool) *Arg {
	return a.setSetting(argSettings_Required, yes)
}

// Requires is Arg::requires: sets an argument that is required when this
// one is present
//
// i.e. when using this argument, the following argument *must* be present.
//
// NOTE: Conflicting rules and override rules take precedence over being
// required
func (a *Arg) Requires(argId string) *Arg {
	a.requires = append(a.requires, argRequire{predicate: argPredicateIsPresent, id: Id(argId)})
	return a
}

// RequiresReset is Arg::requires(None).
func (a *Arg) RequiresReset() *Arg {
	a.requires = nil
	return a
}

// Exclusive is Arg::exclusive: this argument must be passed alone; it
// conflicts with all other arguments.
func (a *Arg) Exclusive(yes bool) *Arg {
	return a.setSetting(argSettings_Exclusive, yes)
}

// Global is Arg::global: specifies that an argument can be matched to all
// child Subcommands.
//
// NOTE: Global arguments *only* propagate down, **not** up (to parent
// commands), however their propagation rules are otherwise unrestricted.
//
// NOTE: Global arguments *cannot* be required.
func (a *Arg) Global(yes bool) *Arg {
	return a.setSetting(argSettings_Global, yes)
}

func (a *Arg) isSet(s argSettings) bool {
	return a.settings.isSet(s)
}

func (a *Arg) setting(setting argSettings) *Arg {
	a.settings.set(setting)
	return a
}

func (a *Arg) unsetSetting(setting argSettings) *Arg {
	a.settings.unset(setting)
	return a
}

func (a *Arg) setSetting(setting argSettings, yes bool) *Arg {
	if yes {
		return a.setting(setting)
	}
	return a.unsetSetting(setting)
}

// # Value Handling

// Action is Arg::action: specify how to react to an argument when parsing
// it.
//
// ArgAction controls things like
//   - Overwriting previous values with new ones
//   - Appending new values to all previous ones
//   - Counting how many times a flag occurs
//
// The default action is `ArgAction::Set`
func (a *Arg) Action(action ArgAction) *Arg {
	a.action = rs.Some(action)
	return a
}

// ActionReset is Arg::action(None).
func (a *Arg) ActionReset() *Arg {
	a.action = rs.None[ArgAction]()
	return a
}

// ValueParser is Arg::value_parser: specify the typed behavior of the
// argument.
//
// This allows parsing and validating a value before storing it into
// ArgMatches as the given type.
//
// Possible value parsers include:
//   - the ValueParser* functions for auto-selecting a value parser for a
//     given type
//   - Or range expressions like `0..=1` as a shorthand for
//     RangedI64ValueParser
//   - Fn(&str) -> Result<T, E>
//   - `[&str]` and PossibleValuesParser for static enumerated values
//   - BoolishValueParser, and FalseyValueParser for alternative `bool`
//     implementations
//   - NonEmptyStringValueParser for basic validation for strings
//   - or any other TypedValueParser implementation
//
// The default value is ValueParser::string.
func (a *Arg) ValueParser(parser BuilderIntoValueParser) *Arg {
	a.valueParser = rs.Some(parser.IntoValueParser())
	return a
}

// ValueParserReset is Arg::value_parser(None).
func (a *Arg) ValueParserReset() *Arg {
	a.valueParser = rs.None[BuilderValueParser]()
	return a
}

// NumArgs is Arg::num_args: specifies the number of arguments parsed per
// occurrence
//
// For example, if you had a `-f <file>` argument where you wanted exactly 3
// 'files' you would set `.num_args(3)`, and this argument wouldn't be
// satisfied unless the user provided 3 and only 3 values.
//
// Users may specify values for arguments in any of the following methods
//
//   - Using a space such as `-o value` or `--option value`
//   - Using an equals and no space such as `-o=value` or `--option=value`
//   - Use a short and no space such as `-ovalue`
//
// WARNING:
//
// Setting a variable number of values (e.g. `1..=10`) for an argument
// without other details can be dangerous in some circumstances. Because it
// allows one to specify more than one value, and doesn't require a
// delimiter, values may be confused for subcommands or positional
// arguments.
func (a *Arg) NumArgs(qty BuilderValueRange) *Arg {
	a.numVals = rs.Some(qty)
	return a
}

// NumArgsReset is Arg::num_args(None).
func (a *Arg) NumArgsReset() *Arg {
	a.numVals = rs.None[BuilderValueRange]()
	return a
}

// ValueName is Arg::value_name: placeholder for the argument's value in the
// help message / usage.
//
// This name is cosmetic only; the name is **not** used to access arguments.
// This setting can be very helpful when describing the type of input the
// user should be using, such as `FILE`, `INTERFACE`, etc. Although not
// required, it's somewhat convention to use all capital letters for the
// value name.
//
// NOTE: implicitly sets Arg::action(ArgAction::Set)
func (a *Arg) ValueName(name string) *Arg {
	return a.ValueNames(name)
}

// ValueNameReset is Arg::value_name(None).
func (a *Arg) ValueNameReset() *Arg {
	a.valNames = nil
	return a
}

// ValueNames is Arg::value_names: placeholders for the argument's values in
// the help message / usage.
//
// These names are cosmetic only, used for help and usage strings only. The
// names are **not** used to access arguments. The values of the arguments
// are accessed in numeric order (i.e. if you specify two names `one` and
// `two` `one` will be the first matched value, `two` will be the second).
//
// This setting can be very helpful when describing the type of input the
// user should be using, such as `FILE`, `INTERFACE`, etc. Although not
// required, it's somewhat convention to use all capital letters for the
// value name.
//
// TIP: It may help to use Arg::next_line_help(true) if there are long, or
// multiple value names in order to not throw off the help text alignment of
// all options.
//
// NOTE: implicitly sets Arg::action(ArgAction::Set) and
// Arg::num_args(1..).
func (a *Arg) ValueNames(names ...string) *Arg {
	a.valNames = append([]string(nil), names...)
	return a
}

// ValueHint is Arg::value_hint: provide the shell a hint about how to
// complete this argument.
//
// See ValueHint for more information.
//
// NOTE: implicitly sets Arg::action(ArgAction::Set)].
func (a *Arg) ValueHint(valueHint ValueHint) *Arg {
	a.valueHint = rs.Some(valueHint)
	return a
}

// ValueHintReset is Arg::value_hint(None).
func (a *Arg) ValueHintReset() *Arg {
	a.valueHint = rs.None[ValueHint]()
	return a
}

// IgnoreCase is Arg::ignore_case: match values against
// PossibleValuesParser without matching case.
//
// When other arguments are conditionally required based on the value of a
// case-insensitive argument, the equality check done by
// Arg::required_if_eq, Arg::required_if_eq_any, or Arg::required_if_eq_all
// is case-insensitive.
//
// NOTE: Setting this requires taking values
//
// NOTE: To do unicode case folding, enable the `unicode` feature flag.
func (a *Arg) IgnoreCase(yes bool) *Arg {
	return a.setSetting(argSettings_IgnoreCase, yes)
}

// AllowHyphenValues is Arg::allow_hyphen_values: allows values which start
// with a leading hyphen (`-`)
//
// To limit values to just numbers, see allow_negative_numbers.
//
// See also trailing_var_arg.
//
// NOTE: Setting this requires taking values
//
// WARNING: Prior arguments with `allow_hyphen_values(true)` get precedence
// over known flags but known flags get precedence over the next possible
// positional argument with `allow_hyphen_values(true)`. When combined with
// [Arg::num_args(..)], Arg::value_terminator is one way to ensure processing
// stops.
//
// WARNING: Take caution when using this setting combined with another
// argument using Arg::num_args, as this becomes ambiguous `$ prog --arg -- -- val`.
// All three `--, --, val` will be values when the user may have thought the
// second `--` would constitute the normal, "Only positional args follow"
// idiom.
func (a *Arg) AllowHyphenValues(yes bool) *Arg {
	return a.setSetting(argSettings_AllowHyphenValues, yes)
}

// AllowNegativeNumbers is Arg::allow_negative_numbers: allows negative
// numbers to pass as values.
//
// This is similar to Arg::allow_hyphen_values except that it only allows
// numbers, all other undefined leading hyphens will fail to parse.
//
// NOTE: Setting this requires taking values
func (a *Arg) AllowNegativeNumbers(yes bool) *Arg {
	return a.setSetting(argSettings_AllowNegativeNumbers, yes)
}

// RequireEquals is Arg::require_equals: requires that options use the
// `--option=val` syntax
//
// i.e. an equals between the option and associated value.
//
// NOTE: Setting this requires taking values
func (a *Arg) RequireEquals(yes bool) *Arg {
	return a.setSetting(argSettings_RequireEquals, yes)
}

// ValueDelimiter is Arg::value_delimiter: allow grouping of multiple values
// via a delimiter.
//
// i.e. allow values (`val1,val2,val3`) to be parsed as three values
// (`val1`, `val2`, and `val3`) instead of one value (`val1,val2,val3`).
//
// See also Command::dont_delimit_trailing_values.
func (a *Arg) ValueDelimiter(d rune) *Arg {
	a.valDelim = rs.Some(d)
	return a
}

// ValueDelimiterReset is Arg::value_delimiter(None).
func (a *Arg) ValueDelimiterReset() *Arg {
	a.valDelim = rs.None[rune]()
	return a
}

// ValueTerminator is Arg::value_terminator: sentinel to **stop** parsing
// multiple values of a given argument.
//
// By default when one sets [num_args(1..)] on an argument, clap will
// continue parsing values for that argument until it reaches another valid
// argument, or one of the other more specific settings for multiple values
// is used (such as num_args).
//
// NOTE: This setting only applies to options and positional arguments
//
// NOTE: When the terminator is passed in on the command line, it is **not**
// stored as one of the values
func (a *Arg) ValueTerminator(term string) *Arg {
	a.terminator = rs.Some(term)
	return a
}

// ValueTerminatorReset is Arg::value_terminator(None).
func (a *Arg) ValueTerminatorReset() *Arg {
	a.terminator = rs.None[string]()
	return a
}

// Raw is Arg::raw: consume all following arguments.
//
// Do not parse them individually, but rather pass them in entirety.
//
// It is worth noting that setting this requires all values to come after a
// `--` to indicate they should all be captured. For example:
//
//	--foo something -- -v -v -v -b -b -b --baz -q -u -x
//
// Will result in everything after `--` to be considered one raw argument.
// This behavior may not be exactly what you are expecting and using
// Arg::trailing_var_arg may be more appropriate.
//
// NOTE: Implicitly sets Arg::action(ArgAction::Set) Arg::num_args(1..),
// Arg::allow_hyphen_values(true), and Arg::last(true) when set to `true`.
func (a *Arg) Raw(yes bool) *Arg {
	if yes {
		if a.numVals.IsNone() {
			a.numVals = rs.Some(BuilderValueRangeFromRangeFrom(1))
		}
	}
	return a.AllowHyphenValues(yes).Last(yes)
}

// DefaultValue is Arg::default_value: value for the argument when not
// present.
//
// Like with command-line values, this will be split by
// Arg::value_delimiter.
//
// NOTE: If the user *does not* use this argument at runtime
// ArgMatches::contains_id will still return `true`. If you wish to
// determine whether the argument was used at runtime or not, consider
// ArgMatches::value_source.
//
// NOTE: This setting is perfectly compatible with Arg::default_value_if but
// slightly different. `Arg::default_value` *only* takes effect when the
// user has not provided this arg at runtime. `Arg::default_value_if`
// however only takes effect when the user has not provided a value at
// runtime **and** these other conditions are met as well. If you have set
// `Arg::default_value` and `Arg::default_value_if`, and the user **did
// not** provide this arg at runtime, nor were the conditions met for
// `Arg::default_value_if`, the `Arg::default_value` will be applied.
func (a *Arg) DefaultValue(val string) *Arg {
	return a.DefaultValues(val)
}

// DefaultValueReset is Arg::default_value(None).
func (a *Arg) DefaultValueReset() *Arg {
	a.defaultVals = nil
	return a
}

// DefaultValues is Arg::default_values: value for the argument when not
// present.
//
// See Arg::default_value.
func (a *Arg) DefaultValues(vals ...string) *Arg {
	a.defaultVals = append([]string(nil), vals...)
	return a
}

// DefaultMissingValue is Arg::default_missing_value: value for the argument
// when the flag is present but no value is specified.
//
// This configuration option is often used to give the user a shortcut and
// allow them to efficiently specify an option argument without requiring an
// explicitly value. The `--color` argument is a common example. By
// supplying a default, such as `default_missing_value("always")`, the user
// can quickly just add `--color` to the command line to produce the desired
// color output.
//
// Like with command-line values, this will be split by
// Arg::value_delimiter.
//
// NOTE: using this configuration option requires the use of the
// .num_args(0..N) and the .require_equals(true) configuration option. These
// are required in order to unambiguously determine what, if any, value was
// supplied for the argument.
func (a *Arg) DefaultMissingValue(val string) *Arg {
	return a.DefaultMissingValuesOs(val)
}

// DefaultMissingValueReset is Arg::default_missing_value(None).
func (a *Arg) DefaultMissingValueReset() *Arg {
	a.defaultMissingVals = nil
	return a
}

// DefaultMissingValueOs is Arg::default_missing_value_os: value for the
// argument when the flag is present but no value is specified.
//
// See Arg::default_missing_value.
func (a *Arg) DefaultMissingValueOs(val string) *Arg {
	return a.DefaultMissingValuesOs(val)
}

// DefaultMissingValues is Arg::default_missing_values: value for the
// argument when the flag is present but no value is specified.
//
// See Arg::default_missing_value.
func (a *Arg) DefaultMissingValues(vals ...string) *Arg {
	return a.DefaultMissingValuesOs(vals...)
}

// DefaultMissingValuesOs is Arg::default_missing_values_os: value for the
// argument when the flag is present but no value is specified.
//
// See Arg::default_missing_values.
func (a *Arg) DefaultMissingValuesOs(vals ...string) *Arg {
	a.defaultMissingVals = append([]string(nil), vals...)
	return a
}

// Env is Arg::env: read from `name` environment variable when argument is
// not present.
//
// If it is not present in the environment, then default rules will apply.
//
// If user sets the argument in the environment:
//   - When Arg::action(ArgAction::Set) is not set, the flag is considered
//     raised.
//   - When Arg::action(ArgAction::Set) is set, ArgMatches::get_one will
//     return value of the environment variable.
//
// If user doesn't set the argument in the environment:
//   - When Arg::action(ArgAction::Set) is not set, the flag is considered
//     off.
//   - When Arg::action(ArgAction::Set) is set, ArgMatches::get_one will
//     return the default specified.
//
// Like with command-line values, this will be split by
// Arg::value_delimiter.
//
// The variable is read when this method is called, as in Rust: when the
// command is defined, not when it parses.
func (a *Arg) Env(name string) *Arg {
	value, ok := os.LookupEnv(name)
	a.env = rs.Some(argEnv{name: name, value: rs.OptionOf(value, ok)})
	return a
}

// EnvReset is Arg::env(None).
func (a *Arg) EnvReset() *Arg {
	a.env = rs.None[argEnv]()
	return a
}

// # Help

// Help is Arg::help: sets the description of the argument for short help
// (`-h`).
//
// Typically, this is a short (one line) description of the arg.
//
// If Arg::long_help is not specified, this message will be displayed for
// `--help`.
//
// NOTE: Only `Arg::help` is used in completion script generation in order
// to be concise
func (a *Arg) Help(h string) *Arg {
	a.help = rs.Some(BuilderStyledStr(h))
	return a
}

// HelpReset is Arg::help(None).
func (a *Arg) HelpReset() *Arg {
	a.help = rs.None[BuilderStyledStr]()
	return a
}

// LongHelp is Arg::long_help: sets the description of the argument for long
// help (`--help`).
//
// Typically this a more detailed (multi-line) message that describes the
// arg.
//
// If Arg::help is not specified, this message will be displayed for `-h`.
//
// NOTE: Only Arg::help is used in completion script generation in order to
// be concise
func (a *Arg) LongHelp(h string) *Arg {
	a.longHelp = rs.Some(BuilderStyledStr(h))
	return a
}

// LongHelpReset is Arg::long_help(None).
func (a *Arg) LongHelpReset() *Arg {
	a.longHelp = rs.None[BuilderStyledStr]()
	return a
}

// DisplayOrder is Arg::display_order: allows custom ordering of args within
// the help message.
//
// `Arg`s with a lower value will be displayed first in the help message.
// Those with the same display order will be sorted.
//
// `Arg`s are automatically assigned a display order based on the order they
// are added to the Command. Overriding this is helpful when the order
// arguments are added in isn't the same as the display order, whether in
// one-off cases or to automatically sort arguments.
//
// NOTE: This setting is ignored for positional arguments which are always
// displayed in index order.
func (a *Arg) DisplayOrder(ord int) *Arg {
	a.dispOrd = rs.Some(ord)
	return a
}

// DisplayOrderReset is Arg::display_order(None).
func (a *Arg) DisplayOrderReset() *Arg {
	a.dispOrd = rs.None[int]()
	return a
}

// HelpHeading is Arg::help_heading: override the `--help` section this
// appears in.
//
// For more on the default help heading, see Command::next_help_heading.
func (a *Arg) HelpHeading(heading string) *Arg {
	a.helpHeading = rs.Some(rs.Some(heading))
	return a
}

// HelpHeadingReset is Arg::help_heading(None): the argument is in no custom
// section.
func (a *Arg) HelpHeadingReset() *Arg {
	a.helpHeading = rs.Some(rs.None[string]())
	return a
}

// NextLineHelp is Arg::next_line_help: render the help on the line after
// the argument.
//
// This can be helpful for arguments with very long or complex help messages.
// This can also be helpful for arguments with very long flag names, or many
// subcommands, or other long descriptions.
//
// NOTE: To apply this setting to all arguments and subcommands, consider
// using crate::Command::next_line_help
func (a *Arg) NextLineHelp(yes bool) *Arg {
	return a.setSetting(argSettings_NextLineHelp, yes)
}

// Hide is Arg::hide: do not display the argument in help message.
//
// NOTE: This does **not** hide the argument from usage strings on error
func (a *Arg) Hide(yes bool) *Arg {
	return a.setSetting(argSettings_Hidden, yes)
}

// HidePossibleValues is Arg::hide_possible_values: do not display the
// possible values in the help message.
//
// This is useful for args with many values, or ones which are explained
// elsewhere in the help text.
//
// To set this for all arguments, see Command::hide_possible_values.
//
// NOTE: Setting this requires taking values
func (a *Arg) HidePossibleValues(yes bool) *Arg {
	return a.setSetting(argSettings_HidePossibleValues, yes)
}

// HideDefaultValue is Arg::hide_default_value: do not display the default
// value of the argument in the help message.
//
// This is useful when default behavior of an arg is explained elsewhere in
// the help text.
//
// NOTE: Setting this requires taking values
func (a *Arg) HideDefaultValue(yes bool) *Arg {
	return a.setSetting(argSettings_HideDefaultValue, yes)
}

// HideEnv is Arg::hide_env: do not display in help the environment variable
// name.
//
// This is useful when the variable option is explained elsewhere in the help
// text.
func (a *Arg) HideEnv(yes bool) *Arg {
	return a.setSetting(argSettings_HideEnv, yes)
}

// HideEnvValues is Arg::hide_env_values: do not display in help any values
// inside the associated ENV variables for the argument.
//
// This is useful when ENV vars contain sensitive values.
func (a *Arg) HideEnvValues(yes bool) *Arg {
	return a.setSetting(argSettings_HideEnvValues, yes)
}

// HideShortHelp is Arg::hide_short_help: hides an argument from short help
// (`-h`).
//
// NOTE: This does **not** hide the argument from usage strings on error
//
// NOTE: Setting this option will cause next-line-help output style to be
// used when long help (`--help`) is called.
func (a *Arg) HideShortHelp(yes bool) *Arg {
	return a.setSetting(argSettings_HiddenShortHelp, yes)
}

// HideLongHelp is Arg::hide_long_help: hides an argument from long help
// (`--help`).
//
// NOTE: This does **not** hide the argument from usage strings on error
//
// NOTE: Setting this option will cause next-line-help output style to be
// used when long help (`--help`) is called.
func (a *Arg) HideLongHelp(yes bool) *Arg {
	return a.setSetting(argSettings_HiddenLongHelp, yes)
}

// # Advanced Argument Relations

// Group is Arg::group: the name of the ArgGroup the argument belongs to.
func (a *Arg) Group(groupId string) *Arg {
	a.groups = append(a.groups, Id(groupId))
	return a
}

// GroupReset is Arg::group(None).
func (a *Arg) GroupReset() *Arg {
	a.groups = nil
	return a
}

// Groups is Arg::groups: the names of ArgGroup's the argument belongs to.
func (a *Arg) Groups(groupIds ...string) *Arg {
	a.groups = append(a.groups, idsFromStrings(groupIds)...)
	return a
}

// DefaultValueIf is Arg::default_value_if: specifies the value of the
// argument if `arg` has been used at runtime.
//
// If `default` is set to `None`, `default_value` will be removed.
//
// Like with command-line values, this will be split by
// Arg::value_delimiter.
//
// NOTE: This setting is perfectly compatible with Arg::default_value but
// slightly different. `Arg::default_value` *only* takes effect when the
// user has not provided this arg at runtime. This setting however only
// takes effect when the user has not provided a value at runtime **and**
// these other conditions are met as well. If you have set
// `Arg::default_value` and `Arg::default_value_if`, and the user **did
// not** provide this arg at runtime, nor were the conditions met for
// `Arg::default_value_if`, the `Arg::default_value` will be applied.
func (a *Arg) DefaultValueIf(argId string, predicate BuilderArgPredicate, default_ rs.Option[string]) *Arg {
	defaults := rs.None[[]string]()
	if value, ok := default_.Get(); ok {
		defaults = rs.Some([]string{value})
	}
	a.defaultValsIfs = append(a.defaultValsIfs, argDefaultIf{id: Id(argId), predicate: predicate, defaults: defaults})
	return a
}

// DefaultValuesIf is Arg::default_values_if: specifies the values of the
// argument if `arg` has been used at runtime.
//
// See Arg::default_value_if.
func (a *Arg) DefaultValuesIf(argId string, predicate BuilderArgPredicate, defaults ...string) *Arg {
	a.defaultValsIfs = append(a.defaultValsIfs, argDefaultIf{
		id: Id(argId), predicate: predicate, defaults: rs.Some(append([]string{}, defaults...))})
	return a
}

// DefaultValueIfs is Arg::default_value_ifs: specifies multiple values and
// conditions in the same manner as Arg::default_value_if.
//
// The method takes a slice of tuples in the `(arg, predicate, default)`
// format.
//
// Like with command-line values, this will be split by
// Arg::value_delimiter.
//
// NOTE: The conditions are stored in order and evaluated in the same order.
// I.e. the first if multiple conditions are true, the first one found will
// be applied and the ultimate value.
func (a *Arg) DefaultValueIfs(ifs ...rs.Tuple3[string, BuilderArgPredicate, rs.Option[string]]) *Arg {
	for _, item := range ifs {
		a = a.DefaultValueIf(item.F0, item.F1, item.F2)
	}
	return a
}

// DefaultValuesIfs is Arg::default_values_ifs: specifies multiple values
// and conditions in the same manner as Arg::default_values_if.
//
// See Arg::default_value_ifs.
func (a *Arg) DefaultValuesIfs(ifs ...rs.Tuple3[string, BuilderArgPredicate, []string]) *Arg {
	for _, item := range ifs {
		a = a.DefaultValuesIf(item.F0, item.F1, item.F2...)
	}
	return a
}

// RequiredUnlessPresent is Arg::required_unless_present: set this arg as
// required as long as the specified argument is not present at runtime.
//
// TIP: Using `Arg::required_unless_present` implies Arg::required and is
// therefore not mandatory to also set.
func (a *Arg) RequiredUnlessPresent(argId string) *Arg {
	a.rUnless = append(a.rUnless, Id(argId))
	return a
}

// RequiredUnlessPresentReset is Arg::required_unless_present(None).
func (a *Arg) RequiredUnlessPresentReset() *Arg {
	a.rUnless = nil
	return a
}

// RequiredUnlessPresentAll is Arg::required_unless_present_all: sets this
// arg as required unless *all* of the specified arguments are present at
// runtime.
//
// In other words, parsing will succeed only if user either
//   - supplies the `self` arg.
//   - supplies *all* of the `names` arguments.
//
// NOTE: If you wish for this argument to only be required unless *any of*
// these args are present see Arg::required_unless_present_any
func (a *Arg) RequiredUnlessPresentAll(names ...string) *Arg {
	a.rUnlessAll = append(a.rUnlessAll, idsFromStrings(names)...)
	return a
}

// RequiredUnlessPresentAny is Arg::required_unless_present_any: sets this
// arg as required unless *any* of the specified arguments are present at
// runtime.
//
// In other words, parsing will succeed only if user either
//   - supplies the `self` arg.
//   - supplies *one or more* of the `unless` arguments.
//
// NOTE: If you wish for this argument to be required unless *all of* these
// args are present see Arg::required_unless_present_all
func (a *Arg) RequiredUnlessPresentAny(names ...string) *Arg {
	a.rUnless = append(a.rUnless, idsFromStrings(names)...)
	return a
}

// RequiredIfEq is Arg::required_if_eq: this argument is required only if the
// specified `arg` is present at runtime and its value equals `val`.
func (a *Arg) RequiredIfEq(argId string, val string) *Arg {
	a.rIfs = append(a.rIfs, argIf{id: Id(argId), val: val})
	return a
}

// RequiredIfEqAny is Arg::required_if_eq_any: specify this argument is
// required based on multiple conditions.
//
// The conditions are set up in a `(arg, val)` style tuple. The requirement
// will only become valid if one of the specified `arg`'s value equals its
// corresponding `val`.
func (a *Arg) RequiredIfEqAny(ifs ...rs.Tuple2[string, string]) *Arg {
	for _, item := range ifs {
		a.rIfs = append(a.rIfs, argIf{id: Id(item.F0), val: item.F1})
	}
	return a
}

// RequiredIfEqAll is Arg::required_if_eq_all: specify this argument is
// required based on multiple conditions.
//
// The conditions are set up in a `(arg, val)` style tuple. The requirement
// will only become valid if every one of the specified `arg`'s value equals
// its corresponding `val`.
func (a *Arg) RequiredIfEqAll(ifs ...rs.Tuple2[string, string]) *Arg {
	for _, item := range ifs {
		a.rIfsAll = append(a.rIfsAll, argIf{id: Id(item.F0), val: item.F1})
	}
	return a
}

// RequiresIf is Arg::requires_if: require another argument if this arg
// matches the ArgPredicate.
//
// This method takes `value, another_arg` pair. At runtime, clap will check
// if this arg (`self`) matches the ArgPredicate. If it does, `another_arg`
// will be marked as required.
func (a *Arg) RequiresIf(val BuilderArgPredicate, argId string) *Arg {
	a.requires = append(a.requires, argRequire{predicate: val, id: Id(argId)})
	return a
}

// RequiresIfs is Arg::requires_ifs: allows multiple conditional
// requirements.
//
// The requirement will only become valid if this arg's value matches the
// ArgPredicate.
func (a *Arg) RequiresIfs(ifs ...rs.Tuple2[BuilderArgPredicate, string]) *Arg {
	for _, item := range ifs {
		a.requires = append(a.requires, argRequire{predicate: item.F0, id: Id(item.F1)})
	}
	return a
}

// ConflictsWith is Arg::conflicts_with: this argument is mutually exclusive
// with the specified argument.
//
// NOTE: Conflicting rules take precedence over being required by default.
// Conflict rules only need to be set for one of the two arguments, they do
// not need to be set for each.
//
// NOTE: Defining a conflict is two-way, but does *not* need to defined for
// both arguments (i.e. if A conflicts with B, defining
// `A.conflicts_with(B)` is sufficient. You do not need to also do
// `B.conflicts_with(A)`)
//
// NOTE: Arg::conflicts_with_all(names) allows specifying an argument which
// conflicts with more than one argument.
//
// NOTE: Arg::exclusive(true) allows specifying an argument which conflicts
// with every other argument.
//
// NOTE: All arguments implicitly conflict with themselves.
func (a *Arg) ConflictsWith(argId string) *Arg {
	a.blacklist = append(a.blacklist, Id(argId))
	return a
}

// ConflictsWithReset is Arg::conflicts_with(None).
func (a *Arg) ConflictsWithReset() *Arg {
	a.blacklist = nil
	return a
}

// ConflictsWithAll is Arg::conflicts_with_all: this argument is mutually
// exclusive with the specified arguments.
//
// See Arg::conflicts_with.
//
// NOTE: Conflicting rules take precedence over being required by default.
// Conflict rules only need to be set for one of the two arguments, they do
// not need to be set for each.
//
// NOTE: Defining a conflict is two-way, but does *not* need to defined for
// both arguments (i.e. if A conflicts with B, defining
// `A.conflicts_with(B)` is sufficient. You do not need need to also do
// `B.conflicts_with(A)`)
//
// NOTE: Arg::exclusive(true) allows specifying an argument which conflicts
// with every other argument.
func (a *Arg) ConflictsWithAll(names ...string) *Arg {
	a.blacklist = append(a.blacklist, idsFromStrings(names)...)
	return a
}

// OverridesWith is Arg::overrides_with: sets an overridable argument.
//
// i.e. this argument and the following argument will override each other in
// POSIX style (whichever argument was specified at runtime **last**
// "wins")
//
// NOTE: When an argument is overridden it is essentially as if it never was
// used, any conflicts, requirements, etc. are evaluated **after** all
// "overrides" have been removed
//
// NOTE: Overriding an argument implies they conflict.
func (a *Arg) OverridesWith(argId string) *Arg {
	a.overrides = append(a.overrides, Id(argId))
	return a
}

// OverridesWithReset is Arg::overrides_with(None).
func (a *Arg) OverridesWithReset() *Arg {
	a.overrides = nil
	return a
}

// OverridesWithAll is Arg::overrides_with_all: sets multiple
// mutually overridable arguments by name.
//
// i.e. this argument and the following argument will override each other in
// POSIX style (whichever argument was specified at runtime **last**
// "wins")
//
// NOTE: When an argument is overridden it is essentially as if it never was
// used, any conflicts, requirements, etc. are evaluated **after** all
// "overrides" have been removed
//
// NOTE: Overriding an argument implies they conflict.
func (a *Arg) OverridesWithAll(names ...string) *Arg {
	a.overrides = append(a.overrides, idsFromStrings(names)...)
	return a
}

// # Reflection

// GetId is Arg::get_id: get the name of the argument.
func (a *Arg) GetId() Id {
	return a.id
}

// GetHelp is Arg::get_help: get the help specified for this argument, if
// any.
func (a *Arg) GetHelp() rs.Option[BuilderStyledStr] {
	return a.help
}

// GetLongHelp is Arg::get_long_help: get the long help specified for this
// argument, if any.
func (a *Arg) GetLongHelp() rs.Option[BuilderStyledStr] {
	return a.longHelp
}

// GetDisplayOrder is Arg::get_display_order: get the placement within help.
func (a *Arg) GetDisplayOrder() int {
	return a.dispOrd.UnwrapOr(999)
}

// GetHelpHeading is Arg::get_help_heading: get the help heading specified
// for this argument, if any.
func (a *Arg) GetHelpHeading() rs.Option[string] {
	if heading, ok := a.helpHeading.Get(); ok {
		return heading
	}
	return rs.None[string]()
}

// GetShort is Arg::get_short: get the short option name for this argument,
// if any.
func (a *Arg) GetShort() rs.Option[rune] {
	return a.short
}

// GetVisibleShortAliases is Arg::get_visible_short_aliases: get visible
// short aliases for this argument, if any.
func (a *Arg) GetVisibleShortAliases() rs.Option[[]rune] {
	if len(a.shortAliases) == 0 {
		return rs.None[[]rune]()
	}
	visible := []rune{}
	for _, alias := range a.shortAliases {
		if alias.visible {
			visible = append(visible, alias.name)
		}
	}
	return rs.Some(visible)
}

// GetAllShortAliases is Arg::get_all_short_aliases: get *all* short aliases
// for this argument, if any, both visible and hidden.
func (a *Arg) GetAllShortAliases() rs.Option[[]rune] {
	if len(a.shortAliases) == 0 {
		return rs.None[[]rune]()
	}
	all := make([]rune, len(a.shortAliases))
	for i, alias := range a.shortAliases {
		all[i] = alias.name
	}
	return rs.Some(all)
}

// GetShortAndVisibleAliases is Arg::get_short_and_visible_aliases: get the
// short option name and its visible aliases, if any.
func (a *Arg) GetShortAndVisibleAliases() rs.Option[[]rune] {
	short, ok := a.short.Get()
	if !ok {
		return rs.None[[]rune]()
	}
	shorts := []rune{short}
	if aliases, ok := a.GetVisibleShortAliases().Get(); ok {
		shorts = append(shorts, aliases...)
	}
	return rs.Some(shorts)
}

// GetLong is Arg::get_long: get the long option name for this argument, if
// any.
func (a *Arg) GetLong() rs.Option[string] {
	return a.long
}

// GetVisibleAliases is Arg::get_visible_aliases: get visible aliases for
// this argument, if any.
func (a *Arg) GetVisibleAliases() rs.Option[[]string] {
	if len(a.aliases) == 0 {
		return rs.None[[]string]()
	}
	visible := []string{}
	for _, alias := range a.aliases {
		if alias.visible {
			visible = append(visible, alias.name)
		}
	}
	return rs.Some(visible)
}

// GetAllAliases is Arg::get_all_aliases: get *all* aliases for this
// argument, if any, both visible and hidden.
func (a *Arg) GetAllAliases() rs.Option[[]string] {
	if len(a.aliases) == 0 {
		return rs.None[[]string]()
	}
	all := make([]string, len(a.aliases))
	for i, alias := range a.aliases {
		all[i] = alias.name
	}
	return rs.Some(all)
}

// GetLongAndVisibleAliases is Arg::get_long_and_visible_aliases: get the
// long option name and its visible aliases, if any.
func (a *Arg) GetLongAndVisibleAliases() rs.Option[[]string] {
	long, ok := a.GetLong().Get()
	if !ok {
		return rs.None[[]string]()
	}
	longs := []string{long}
	if aliases, ok := a.GetVisibleAliases().Get(); ok {
		longs = append(longs, aliases...)
	}
	return rs.Some(longs)
}

// GetAliases is Arg::get_aliases: get hidden aliases for this argument, if
// any.
func (a *Arg) GetAliases() rs.Option[[]string] {
	if len(a.aliases) == 0 {
		return rs.None[[]string]()
	}
	hidden := []string{}
	for _, alias := range a.aliases {
		if !alias.visible {
			hidden = append(hidden, alias.name)
		}
	}
	return rs.Some(hidden)
}

// GetPossibleValues is Arg::get_possible_values: get the names of possible
// values for this argument. Only useful for user facing applications, such
// as building help messages or man files.
func (a *Arg) GetPossibleValues() []BuilderPossibleValue {
	if !a.isTakesValueSet() {
		return []BuilderPossibleValue{}
	}
	return a.GetValueParser().PossibleValues().UnwrapOr(nil)
}

// GetValueNames is Arg::get_value_names: get the names of values for this
// argument.
func (a *Arg) GetValueNames() rs.Option[[]string] {
	if len(a.valNames) == 0 {
		return rs.None[[]string]()
	}
	return rs.Some(a.valNames)
}

// GetNumArgs is Arg::get_num_args: get the number of values for this
// argument.
func (a *Arg) GetNumArgs() rs.Option[BuilderValueRange] {
	return a.numVals
}

func (a *Arg) getMinVals() int {
	return a.GetNumArgs().Expect(internalErrorMsg).MinValues()
}

// GetValueDelimiter is Arg::get_value_delimiter: get the delimiter between
// multiple values.
func (a *Arg) GetValueDelimiter() rs.Option[rune] {
	return a.valDelim
}

// GetValueTerminator is Arg::get_value_terminator: get the value
// terminator for this argument. The `value_terminator` is a value that
// terminates parsing of multi-valued arguments.
func (a *Arg) GetValueTerminator() rs.Option[string] {
	return a.terminator
}

// GetIndex is Arg::get_index: get the index of this argument, if any.
func (a *Arg) GetIndex() rs.Option[int] {
	return a.index
}

// GetValueHint is Arg::get_value_hint: get the value hint of this argument.
func (a *Arg) GetValueHint() ValueHint {
	if hint, ok := a.valueHint.Get(); ok {
		return hint
	}
	if a.isTakesValueSet() {
		typeId := a.GetValueParser().TypeId()
		if typeId.typeName == "std::path::PathBuf" {
			return ValueHint_AnyPath
		}
		return ValueHintDefault()
	}
	return ValueHintDefault()
}

// GetEnv is Arg::get_env: get the environment variable name specified for
// this argument, if any.
func (a *Arg) GetEnv() rs.Option[string] {
	if env, ok := a.env.Get(); ok {
		return rs.Some(env.name)
	}
	return rs.None[string]()
}

// GetDefaultValues is Arg::get_default_values: get the default values
// specified for this argument, if any.
func (a *Arg) GetDefaultValues() []string {
	return a.defaultVals
}

// IsPositional is Arg::is_positional: checks whether this argument is a
// positional or not.
func (a *Arg) IsPositional() bool {
	return a.GetLong().IsNone() && a.GetShort().IsNone()
}

// IsRequiredSet is Arg::is_required_set: reports whether Arg::required is
// set.
func (a *Arg) IsRequiredSet() bool {
	return a.isSet(argSettings_Required)
}

func (a *Arg) isMultipleValuesSet() bool {
	return a.GetNumArgs().UnwrapOr(BuilderValueRangeDefault()).isMultiple()
}

func (a *Arg) isTakesValueSet() bool {
	return a.GetNumArgs().UnwrapOr(BuilderValueRangeFromUsize(1)).TakesValues()
}

// IsAllowHyphenValuesSet is Arg::is_allow_hyphen_values_set: report whether
// Arg::allow_hyphen_values is set.
func (a *Arg) IsAllowHyphenValuesSet() bool {
	return a.isSet(argSettings_AllowHyphenValues)
}

// IsAllowNegativeNumbersSet is Arg::is_allow_negative_numbers_set: report
// whether Arg::allow_negative_numbers is set.
func (a *Arg) IsAllowNegativeNumbersSet() bool {
	return a.isSet(argSettings_AllowNegativeNumbers)
}

// GetAction is Arg::get_action: behavior when parsing the argument.
func (a *Arg) GetAction() ArgAction {
	return a.action.UnwrapOr(ArgAction_Set)
}

// GetValueParser is Arg::get_value_parser: configured parser for argument
// values.
func (a *Arg) GetValueParser() BuilderValueParser {
	if valueParser, ok := a.valueParser.Get(); ok {
		return valueParser
	}
	return BuilderValueParserString()
}

// IsGlobalSet is Arg::is_global_set: report whether Arg::global is set.
func (a *Arg) IsGlobalSet() bool {
	return a.isSet(argSettings_Global)
}

// IsNextLineHelpSet is Arg::is_next_line_help_set: report whether
// Arg::next_line_help is set.
func (a *Arg) IsNextLineHelpSet() bool {
	return a.isSet(argSettings_NextLineHelp)
}

// IsHideSet is Arg::is_hide_set: report whether Arg::hide is set.
func (a *Arg) IsHideSet() bool {
	return a.isSet(argSettings_Hidden)
}

// IsHideDefaultValueSet is Arg::is_hide_default_value_set: report whether
// Arg::hide_default_value is set.
func (a *Arg) IsHideDefaultValueSet() bool {
	return a.isSet(argSettings_HideDefaultValue)
}

// IsHidePossibleValuesSet is Arg::is_hide_possible_values_set: report
// whether Arg::hide_possible_values is set.
func (a *Arg) IsHidePossibleValuesSet() bool {
	return a.isSet(argSettings_HidePossibleValues)
}

// IsHideEnvSet is Arg::is_hide_env_set: report whether Arg::hide_env is
// set.
func (a *Arg) IsHideEnvSet() bool {
	return a.isSet(argSettings_HideEnv)
}

// IsHideEnvValuesSet is Arg::is_hide_env_values_set: report whether
// Arg::hide_env_values is set.
func (a *Arg) IsHideEnvValuesSet() bool {
	return a.isSet(argSettings_HideEnvValues)
}

// IsHideShortHelpSet is Arg::is_hide_short_help_set: report whether
// Arg::hide_short_help is set.
func (a *Arg) IsHideShortHelpSet() bool {
	return a.isSet(argSettings_HiddenShortHelp)
}

// IsHideLongHelpSet is Arg::is_hide_long_help_set: report whether
// Arg::hide_long_help is set.
func (a *Arg) IsHideLongHelpSet() bool {
	return a.isSet(argSettings_HiddenLongHelp)
}

// IsRequireEqualsSet is Arg::is_require_equals_set: report whether
// Arg::require_equals is set.
func (a *Arg) IsRequireEqualsSet() bool {
	return a.isSet(argSettings_RequireEquals)
}

// IsExclusiveSet is Arg::is_exclusive_set: reports whether Arg::exclusive
// is set.
func (a *Arg) IsExclusiveSet() bool {
	return a.isSet(argSettings_Exclusive)
}

// IsTrailingVarArgSet is Arg::is_trailing_var_arg_set: report whether
// Arg::trailing_var_arg is set.
func (a *Arg) IsTrailingVarArgSet() bool {
	return a.isSet(argSettings_TrailingVarArg)
}

// IsLastSet is Arg::is_last_set: reports whether Arg::last is set.
func (a *Arg) IsLastSet() bool {
	return a.isSet(argSettings_Last)
}

// IsIgnoreCaseSet is Arg::is_ignore_case_set: reports whether
// Arg::ignore_case is set.
func (a *Arg) IsIgnoreCaseSet() bool {
	return a.isSet(argSettings_IgnoreCase)
}

// # Internally used only

func (a *Arg) build() {
	if a.action.IsNone() {
		if numVals, ok := a.numVals.Get(); ok && numVals == BuilderValueRangeEmpty {
			action := ArgAction_SetTrue
			a.action = rs.Some(action)
		} else {
			var action ArgAction
			if a.IsPositional() && a.numVals.UnwrapOr(BuilderValueRangeDefault()).isUnbounded() {
				// Allow collecting arguments interleaved with flags
				//
				// Bounded values are probably a group and the user should explicitly opt-in to
				// Append
				action = ArgAction_Append
			} else {
				action = ArgAction_Set
			}
			a.action = rs.Some(action)
		}
	}
	if action, ok := a.action.Get(); ok {
		if defaultValue, ok := action.defaultValue().Get(); ok {
			if len(a.defaultVals) == 0 {
				a.defaultVals = []string{defaultValue}
			}
		}
		if defaultValue, ok := action.defaultMissingValue().Get(); ok {
			if len(a.defaultMissingVals) == 0 {
				a.defaultMissingVals = []string{defaultValue}
			}
		}
	}

	if a.valueParser.IsNone() {
		if def, ok := a.action.Unwrap().defaultValueParser().Get(); ok {
			a.valueParser = rs.Some(def)
		} else {
			a.valueParser = rs.Some(BuilderValueParserString())
		}
	}

	valNamesLen := len(a.valNames)
	if valNamesLen > 1 {
		if a.numVals.IsNone() {
			a.numVals = rs.Some(BuilderValueRangeFromUsize(valNamesLen))
		}
	} else {
		nargs := a.GetAction().defaultNumArgs()
		if a.numVals.IsNone() {
			a.numVals = rs.Some(nargs)
		}
	}
}

// nameNoBrackets is used for positionals when printing.
func (a *Arg) nameNoBrackets() string {
	delim := " "
	if len(a.valNames) != 0 {
		if len(a.valNames) > 1 {
			parts := make([]string, len(a.valNames))
			for i, n := range a.valNames {
				parts[i] = "<" + n + ">"
			}
			return strings.Join(parts, delim)
		}
		return a.valNames[0]
	}
	return a.GetId().AsStr()
}

func (a *Arg) stylized(styles *BuilderStyles, required rs.Option[bool]) BuilderStyledStr {
	literal := styles.GetLiteral()

	styled := BuilderStyledStrNew()
	// Write the name such --long or -l
	if l, ok := a.GetLong().Get(); ok {
		styled.pushString(literal.Render() + "--" + l + literal.RenderReset())
	} else if s, ok := a.GetShort().Get(); ok {
		styled.pushString(literal.Render() + "-" + string(s) + literal.RenderReset())
	}
	styled.pushStyled(a.stylizeArgSuffix(styles, required))
	return styled
}

func (a *Arg) stylizeArgSuffix(styles *BuilderStyles, required rs.Option[bool]) BuilderStyledStr {
	literal := styles.GetLiteral()
	placeholder := styles.GetPlaceholder()
	styled := BuilderStyledStrNew()

	needClosingBracket := false
	if a.isTakesValueSet() && !a.IsPositional() {
		isOptionalVal := a.getMinVals() == 0
		var style BuilderStylingStyle
		var start string
		if a.IsRequireEqualsSet() {
			if isOptionalVal {
				needClosingBracket = true
				style, start = placeholder, "[="
			} else {
				style, start = literal, "="
			}
		} else if isOptionalVal {
			needClosingBracket = true
			style, start = placeholder, " ["
		} else {
			style, start = placeholder, " "
		}
		styled.pushString(style.Render() + start + style.RenderReset())
	}
	if a.isTakesValueSet() || a.IsPositional() {
		isRequired := required.UnwrapOrElse(a.IsRequiredSet)
		argVal := a.renderArgVal(isRequired)
		styled.pushString(placeholder.Render() + argVal + placeholder.RenderReset())
	} else if a.GetAction() == ArgAction_Count {
		styled.pushString(placeholder.Render() + "..." + placeholder.RenderReset())
	}
	if needClosingBracket {
		styled.pushString(placeholder.Render() + "]" + placeholder.RenderReset())
	}

	return styled
}

func (a *Arg) renderArgVal(required bool) string {
	var rendered strings.Builder

	numVals := a.GetNumArgs().UnwrapOr(BuilderValueRangeFromUsize(1))

	var valNames []string
	if len(a.valNames) == 0 {
		valNames = []string{a.id.AsStr()}
	} else {
		valNames = append([]string(nil), a.valNames...)
	}
	if len(valNames) == 1 {
		minimum := max(numVals.MinValues(), 1)
		valName := valNames[0]
		valNames = make([]string, minimum)
		for i := range valNames {
			valNames[i] = valName
		}
	}

	if debugAssertions && !a.isTakesValueSet() {
		rs.PanicAt(1, "assertion failed: self.is_takes_value_set()")
	}
	for n, valName := range valNames {
		var argName string
		if a.IsPositional() && (numVals.MinValues() == 0 || !required) {
			argName = "[" + valName + "]"
		} else {
			argName = "<" + valName + ">"
		}

		if n != 0 {
			rendered.WriteByte(' ')
		}
		rendered.WriteString(argName)
	}

	extraValues := false
	extraValues = extraValues || len(valNames) < numVals.MaxValues()
	if a.IsPositional() && a.GetAction() == ArgAction_Append {
		extraValues = true
	}
	if extraValues {
		rendered.WriteString("...")
	}

	return rendered.String()
}

// isMultiple reports either multiple values or occurrences.
func (a *Arg) isMultiple() bool {
	return a.isMultipleValuesSet() || a.GetAction() == ArgAction_Append
}

// Clone is `impl Clone for Arg`; `Arg::from(&arg)` is the same.
func (a *Arg) Clone() *Arg {
	cloned := *a
	cloned.blacklist = cloneIds(a.blacklist)
	cloned.overrides = cloneIds(a.overrides)
	cloned.groups = cloneIds(a.groups)
	cloned.requires = append([]argRequire(nil), a.requires...)
	cloned.rIfs = append([]argIf(nil), a.rIfs...)
	cloned.rIfsAll = append([]argIf(nil), a.rIfsAll...)
	cloned.rUnless = cloneIds(a.rUnless)
	cloned.rUnlessAll = cloneIds(a.rUnlessAll)
	cloned.aliases = append([]argAlias(nil), a.aliases...)
	cloned.shortAliases = append([]argShortAlias(nil), a.shortAliases...)
	cloned.valNames = append([]string(nil), a.valNames...)
	cloned.defaultVals = append([]string(nil), a.defaultVals...)
	cloned.defaultValsIfs = append([]argDefaultIf(nil), a.defaultValsIfs...)
	cloned.defaultMissingVals = append([]string(nil), a.defaultMissingVals...)
	return &cloned
}

// Equal is `impl PartialEq for Arg`: by id.
func (a *Arg) Equal(other *Arg) bool {
	return a.GetId() == other.GetId()
}

// Compare is `impl Ord for Arg`: by id.
func (a *Arg) Compare(other *Arg) int {
	return strings.Compare(string(a.GetId()), string(other.GetId()))
}

// String is `impl Display for Arg`.
func (a *Arg) String() string {
	plain := BuilderStylesPlain()
	return a.stylized(&plain, rs.None[bool]()).String()
}

// FmtDisplay is `impl Display for Arg`.
func (a *Arg) FmtDisplay(f *rs.Formatter) error {
	plain := BuilderStylesPlain()
	return a.stylized(&plain, rs.None[bool]()).FmtDisplay(f)
}

const internalErrorMsg = "Fatal internal error. Please consider filing a bug " +
	"report at https://github.com/clap-rs/clap/issues"
