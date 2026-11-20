// Port of clap_builder 4.6.0 src/builder/command.rs

package clap

import (
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// commandAlias is one `(Str, bool)` of Command::aliases: (name, visible).
type commandAlias struct {
	name    string
	visible bool
}

// commandShortAlias is one `(char, bool)`: (name, visible).
type commandShortAlias struct {
	name    rune
	visible bool
}

// builderAppExt is Command's `app_ext`: the extensions a command hands down
// to its subcommands. They are Styles, TermWidth and MaxTermWidth.
type builderAppExt struct {
	styles       rs.Option[BuilderStyles]
	termWidth    rs.Option[int]
	maxTermWidth rs.Option[int]
}

// update is Extensions::update: every extension of other replaces the one
// held.
func (e *builderAppExt) update(other *builderAppExt) {
	if other.styles.IsSome() {
		e.styles = other.styles
	}
	if other.termWidth.IsSome() {
		e.termWidth = other.termWidth
	}
	if other.maxTermWidth.IsSome() {
		e.maxTermWidth = other.maxTermWidth
	}
}

// Command is clap::Command: build a command-line interface.
//
// This includes defining arguments, subcommands, parser behavior, and help
// output. Once all configuration is complete, the Command::get_matches
// family of methods starts the runtime-parsing process. These methods then
// return information about the user supplied arguments (or lack thereof).
//
// When deriving a Parser, you can use CommandFactory::command to access the
// Command.
//
//   - Basic API
//   - Application-wide Settings
//   - Command-specific Settings
//   - Subcommand-specific Settings
//   - Reflection
//
// The builder methods change the command and return it, so calls chain as
// they do in Rust. Where Rust passes `&cmd` (which clones), pass
// cmd.Clone().
//
// A method whose Rust parameter is `impl IntoResettable<T>` takes the value;
// passing `None` is the method of the same name with `Reset` appended.
type Command struct {
	name                string
	longFlag            rs.Option[string]
	shortFlag           rs.Option[rune]
	displayName         rs.Option[string]
	binName             rs.Option[string]
	author              rs.Option[string]
	version             rs.Option[string]
	longVersion         rs.Option[string]
	about               rs.Option[BuilderStyledStr]
	longAbout           rs.Option[BuilderStyledStr]
	beforeHelp          rs.Option[BuilderStyledStr]
	beforeLongHelp      rs.Option[BuilderStyledStr]
	afterHelp           rs.Option[BuilderStyledStr]
	afterLongHelp       rs.Option[BuilderStyledStr]
	aliases             []commandAlias      // (name, visible)
	shortFlagAliases    []commandShortAlias // (name, visible)
	longFlagAliases     []commandAlias      // (name, visible)
	usageStr            rs.Option[BuilderStyledStr]
	usageName           rs.Option[string]
	helpStr             rs.Option[BuilderStyledStr]
	dispOrd             rs.Option[int]
	template            rs.Option[BuilderStyledStr]
	settings            appFlags
	gSettings           appFlags
	args                mkeymapMKeyMap
	subcommands         []*Command
	groups              []*ArgGroup
	currentHelpHeading  rs.Option[string]
	currentDispOrd      rs.Option[int]
	subcommandValueName rs.Option[string]
	subcommandHeading   rs.Option[string]
	externalValueParser rs.Option[BuilderValueParser]
	longHelpExists      bool
	deferred            rs.Option[func(*Command) *Command]
	appExt              builderAppExt
}

// # Basic API

// CommandNew is Command::new: creates a new instance of an Command.
//
// It is common, but not required, to use binary name as the `name`. This
// name will only be displayed to the user when they request to print
// version or help and usage information.
//
// See also the `command!` and `crate_name!` macros.
func CommandNew(name string) *Command {
	cmd := CommandDefault()
	cmd.name = name
	return cmd
}

// CommandDefault is `impl Default for Command`.
func CommandDefault() *Command {
	return &Command{
		currentDispOrd: rs.Some(0),
	}
}

// Arg is Command::arg: adds an argument to the list of valid possibilities.
func (c *Command) Arg(a *Arg) *Command {
	c.argInternal(a)
	return c
}

func (c *Command) argInternal(arg *Arg) {
	if currentDispOrd, ok := c.currentDispOrd.Get(); ok {
		if !arg.IsPositional() {
			current := currentDispOrd
			if arg.dispOrd.IsNone() {
				arg.dispOrd = rs.Some(current)
			}
			c.currentDispOrd = rs.Some(current + 1)
		}
	}

	if arg.helpHeading.IsNone() {
		arg.helpHeading = rs.Some(c.currentHelpHeading)
	}
	c.args.push(arg)
}

// Args is Command::args: adds multiple arguments to the list of valid
// possibilities.
func (c *Command) Args(args ...*Arg) *Command {
	for _, arg := range args {
		c = c.Arg(arg)
	}
	return c
}

// MutArg is Command::mut_arg: allows one to mutate an Arg after it's been
// added to a Command.
//
// Panics if the argument is undefined.
func (c *Command) MutArg(argId string, f func(*Arg) *Arg) *Command {
	id := argId
	a, ok := c.args.removeByName(id).Get()
	if !ok {
		rs.PanicAt(1, "Argument `"+id+"` is undefined")
	}

	c.args.push(f(a))
	return c
}

// MutArgs is Command::mut_args: allows one to mutate all Args after they've
// been added to a Command.
//
// This does not affect the built-in `--help` or `--version` arguments.
func (c *Command) MutArgs(f func(*Arg) *Arg) *Command {
	c.args.mutArgs(f)
	return c
}

// MutGroup is Command::mut_group: allows one to mutate an ArgGroup after
// it's been added to a Command.
//
// Panics if the argument is undefined.
func (c *Command) MutGroup(argId string, f func(*ArgGroup) *ArgGroup) *Command {
	id := argId
	index := -1
	for i, g := range c.groups {
		if g.GetId() == Id(id) {
			index = i
			break
		}
	}
	if index < 0 {
		rs.PanicAt(1, "Group `"+id+"` is undefined")
	}
	a := c.groups[index]
	c.groups = append(c.groups[:index:index], c.groups[index+1:]...)

	c.groups = append(c.groups, f(a))
	return c
}

// MutSubcommand is Command::mut_subcommand: allows one to mutate a Command
// after it's been added as a subcommand.
//
// This can be useful for modifying auto-generated arguments of nested
// subcommands with Command::mut_arg.
//
// Panics if the subcommand is undefined.
func (c *Command) MutSubcommand(name string, f func(*Command) *Command) *Command {
	pos := -1
	for i, s := range c.subcommands {
		if s.name == name {
			pos = i
			break
		}
	}

	if pos < 0 {
		rs.PanicAt(1, "Command `"+name+"` is undefined")
	}
	subcmd := c.subcommands[pos]
	c.subcommands = append(c.subcommands[:pos:pos], c.subcommands[pos+1:]...)

	c.subcommands = append(c.subcommands, f(subcmd))
	return c
}

// MutSubcommands is Command::mut_subcommands: allows one to mutate all
// Commands after they've been added as subcommands.
//
// This does not affect the built-in `--help` or `--version` arguments.
func (c *Command) MutSubcommands(f func(*Command) *Command) *Command {
	subcommands := c.subcommands
	c.subcommands = make([]*Command, 0, len(subcommands))
	for _, sc := range subcommands {
		c.subcommands = append(c.subcommands, f(sc))
	}
	return c
}

// Group is Command::group: adds an ArgGroup to the application.
//
// ArgGroups are a family of related arguments. By placing them in a logical
// group, you can build easier requirement and exclusion rules.
//
// Example use cases:
//   - Make an entire ArgGroup required, meaning that one (and *only* one)
//     argument from that group must be present at runtime.
//   - Name an ArgGroup as a conflict to another argument. Meaning any of
//     the arguments that belong to that group will cause a failure if
//     present with the conflicting argument.
//   - Ensure exclusion between arguments.
//   - Extract a value from a group instead of determining exactly which
//     argument was used.
func (c *Command) Group(group *ArgGroup) *Command {
	c.groups = append(c.groups, group)
	return c
}

// Groups is Command::groups: adds multiple ArgGroups to the Command at
// once.
func (c *Command) Groups(groups ...*ArgGroup) *Command {
	for _, g := range groups {
		c = c.Group(g)
	}
	return c
}

// Subcommand is Command::subcommand: adds a subcommand to the list of valid
// possibilities.
//
// Subcommands are effectively sub-Commands, because they can contain their
// own arguments, subcommands, version, usage, etc. They also function just
// like Commands, in that they get their own auto generated help, version,
// and usage.
//
// A subcommand's Command::name will be used for:
//   - The argument the user passes in
//   - Programmatically looking up the subcommand
func (c *Command) Subcommand(subcmd *Command) *Command {
	return c.subcommandInternal(subcmd)
}

func (c *Command) subcommandInternal(subcmd *Command) *Command {
	if currentDispOrd, ok := c.currentDispOrd.Get(); ok {
		current := currentDispOrd
		if subcmd.dispOrd.IsNone() {
			subcmd.dispOrd = rs.Some(current)
		}
		c.currentDispOrd = rs.Some(current + 1)
	}
	c.subcommands = append(c.subcommands, subcmd)
	return c
}

// Subcommands is Command::subcommands: adds multiple subcommands to the
// list of valid possibilities.
func (c *Command) Subcommands(subcmds ...*Command) *Command {
	for _, subcmd := range subcmds {
		c = c.Subcommand(subcmd)
	}
	return c
}

// Defer is Command::defer: delay initialization for parts of the Command.
//
// This is useful for large applications to delay definitions of
// subcommands until they are being invoked.
func (c *Command) Defer(deferred func(*Command) *Command) *Command {
	c.deferred = rs.Some(deferred)
	return c
}

// DebugAssert is Command::debug_assert: catch problems earlier in the
// development cycle.
//
// Most error states are handled as asserts under the assumption they are
// programming mistake and not something to handle at runtime. Rather than
// relying on tests (manual or automated) that exhaustively test your CLI to
// ensure the asserts are evaluated, this will run those asserts in a way
// convenient for running as a test.
//
// Note: This will not help with asserts in ArgMatches, those will need
// exhaustive testing of your CLI.
func (c *Command) DebugAssert() {
	c.Build()
}

// Error is Command::error: custom error message for post-parsing
// validation.
func (c *Command) Error(kind ErrorErrorKind, message string) *Error {
	return ErrorRaw(kind, message).Format(c)
}

// GetMatches is Command::get_matches: parse `env::args_os`, exiting on
// failure.
//
// Panics if contradictory arguments or settings exist (debug builds).
func (c *Command) GetMatches() *ArgMatches {
	return c.GetMatchesFrom(os.Args)
}

// GetMatchesMut is Command::get_matches_mut: parse `env::args_os`, exiting
// on failure.
//
// Like Command::get_matches but doesn't consume the `Command`.
//
// NOTE: This will not update the binary name in subcommands' usage. See
// Command::try_get_matches_from_mut.
//
// Panics if contradictory arguments or settings exist (debug builds).
func (c *Command) GetMatchesMut() *ArgMatches {
	matches, err := c.tryGetMatchesFromMut(os.Args)
	if err != nil {
		err.Exit()
	}
	return matches
}

// TryGetMatches is Command::try_get_matches: parse `env::args_os`,
// returning a clap::Result on failure.
//
// NOTE: This method WILL NOT exit when `--help` or `--version` (or short
// versions) are used. It will return a clap::Error, where the kind is a
// ErrorKind::DisplayHelp or ErrorKind::DisplayVersion respectively. You
// must call Error::exit or perform a std::process::exit.
//
// Panics if contradictory arguments or settings exist (debug builds).
func (c *Command) TryGetMatches() (*ArgMatches, error) {
	// Start the parsing
	return c.TryGetMatchesFrom(os.Args)
}

// GetMatchesFrom is Command::get_matches_from: parse the specified
// arguments, exiting on failure.
//
// NOTE: The first argument will be parsed as the binary name unless
// Command::no_binary_name is used.
//
// Panics if contradictory arguments or settings exist (debug builds).
func (c *Command) GetMatchesFrom(itr []string) *ArgMatches {
	matches, err := c.tryGetMatchesFromMut(itr)
	if err != nil {
		err.Exit()
	}
	return matches
}

// TryGetMatchesFrom is Command::try_get_matches_from: parse the specified
// arguments, returning a clap::Result on failure.
//
// NOTE: This method WILL NOT exit when `--help` or `--version` (or short
// versions) are used. It will return a clap::Error, where the kind is a
// ErrorKind::DisplayHelp or ErrorKind::DisplayVersion respectively. You
// must call Error::exit or perform a std::process::exit yourself.
//
// NOTE: The first argument will be parsed as the binary name unless
// Command::no_binary_name is used.
//
// Panics if contradictory arguments or settings exist (debug builds).
func (c *Command) TryGetMatchesFrom(itr []string) (*ArgMatches, error) {
	return c.TryGetMatchesFromMut(itr)
}

// TryGetMatchesFromMut is Command::try_get_matches_from_mut: parse the
// specified arguments, returning a clap::Result on failure.
//
// Like Command::try_get_matches_from but doesn't consume the `Command`.
//
// NOTE: This method WILL NOT exit when `--help` or `--version` (or short
// versions) are used. It will return a clap::Error, where the kind is a
// ErrorKind::DisplayHelp or ErrorKind::DisplayVersion respectively. You
// must call Error::exit or perform a std::process::exit yourself.
//
// NOTE: The first argument will be parsed as the binary name unless
// Command::no_binary_name is used.
//
// NOTE: This will not update the binary name in subcommands' usage.
//
// Panics if contradictory arguments or settings exist (debug builds).
func (c *Command) TryGetMatchesFromMut(itr []string) (*ArgMatches, error) {
	matches, err := c.tryGetMatchesFromMut(itr)
	if err != nil {
		return nil, err
	}
	return matches, nil
}

func (c *Command) tryGetMatchesFromMut(itr []string) (*ArgMatches, *Error) {
	rawArgs := lexRawArgsNew(itr)
	cursor := rawArgs.cursor()

	if c.settings.isSet(appSettings_Multicall) {
		if argv0, ok := rawArgs.nextOs(&cursor).Get(); ok {
			if command, ok := pathFileStem(argv0); ok && validUtf8(command) {
				// Stop borrowing command so we can get another mut ref to it.
				rawArgs.insert(&cursor, []string{command})
				c.name = ""
				c.binName = rs.None[string]()
				return c.doParse(rawArgs, cursor)
			}
		}
	}

	// Get the name of the program (argument 1 of env::args()) and determine the
	// actual file
	// that was used to execute the program. This is because a program called
	// ./target/release/my_prog -a
	// will have two arguments, './target/release/my_prog', '-a' but we don't want
	// to display
	// the full path when displaying help messages and such
	if !c.settings.isSet(appSettings_NoBinaryName) {
		if name, ok := rawArgs.nextOs(&cursor).Get(); ok {
			if f, ok := pathFileName(name); ok {
				if validUtf8(f) {
					if c.binName.IsNone() {
						c.binName = rs.Some(f)
					}
				}
			}
		}
	}

	return c.doParse(rawArgs, cursor)
}

// PrintHelp is Command::print_help: prints the short help message (`-h`)
// to io::stdout().
//
// See also Command::print_long_help.
func (c *Command) PrintHelp() error {
	c.buildSelf(false)
	color := c.colorHelp()

	styled := BuilderStyledStrNew()
	usage := outputUsageNew(c)
	outputWriteHelp(&styled, c, usage, false)

	colorizer := outputFmtColorizerNew(outputFmtStream_Stdout, color).withContent(styled)
	return colorizer.print()
}

// PrintLongHelp is Command::print_long_help: prints the long help message
// (`--help`) to io::stdout().
//
// See also Command::print_help.
func (c *Command) PrintLongHelp() error {
	c.buildSelf(false)
	color := c.colorHelp()

	styled := BuilderStyledStrNew()
	usage := outputUsageNew(c)
	outputWriteHelp(&styled, c, usage, true)

	colorizer := outputFmtColorizerNew(outputFmtStream_Stdout, color).withContent(styled)
	return colorizer.print()
}

// RenderHelp is Command::render_help: render the short help message (`-h`)
// to a StyledStr.
//
// See also Command::render_long_help.
func (c *Command) RenderHelp() BuilderStyledStr {
	c.buildSelf(false)

	styled := BuilderStyledStrNew()
	usage := outputUsageNew(c)
	outputWriteHelp(&styled, c, usage, false)
	return styled
}

// RenderLongHelp is Command::render_long_help: render the long help
// message (`--help`) to a StyledStr.
//
// See also Command::render_help.
func (c *Command) RenderLongHelp() BuilderStyledStr {
	c.buildSelf(false)

	styled := BuilderStyledStrNew()
	usage := outputUsageNew(c)
	outputWriteHelp(&styled, c, usage, true)
	return styled
}

// RenderVersion is Command::render_version: version message rendered as if
// the user ran `-V`.
//
// See also Command::render_long_version.
//
// # Coloring
//
// This function does not try to color the message nor it inserts any ANSI
// escape codes.
func (c *Command) RenderVersion() string {
	return c.renderVersion(false)
}

// RenderLongVersion is Command::render_long_version: version message
// rendered as if the user ran `--version`.
//
// See also Command::render_version.
//
// # Coloring
//
// This function does not try to color the message nor it inserts any ANSI
// escape codes.
func (c *Command) RenderLongVersion() string {
	return c.renderVersion(true)
}

// RenderUsage is Command::render_usage: usage statement.
func (c *Command) RenderUsage() BuilderStyledStr {
	return c.renderUsage_().UnwrapOr(BuilderStyledStrNew())
}

func (c *Command) renderUsage_() rs.Option[BuilderStyledStr] {
	// If there are global arguments, or settings we need to propagate them down to subcommands
	// before parsing incase we run into a subcommand
	c.buildSelf(false)

	return outputUsageNew(c).createUsageWithTitle(nil)
}

// # Application-wide Settings
//
// These settings will apply to the top-level command and all subcommands,
// by default. Some settings can be overridden in subcommands.

// NoBinaryName is Command::no_binary_name: specifies that the parser should
// not assume the first argument passed is the binary name.
//
// This is normally the case when using a "daemon" style mode. For shells /
// REPLs, see Command::multicall.
func (c *Command) NoBinaryName(yes bool) *Command {
	return c.setGlobalSetting(appSettings_NoBinaryName, yes)
}

// IgnoreErrors is Command::ignore_errors: try not to fail on parse errors,
// like missing option values.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) IgnoreErrors(yes bool) *Command {
	return c.setGlobalSetting(appSettings_IgnoreErrors, yes)
}

// ArgsOverrideSelf is Command::args_override_self: replace prior
// occurrences of arguments rather than error.
//
// For any argument that would conflict with itself by default (e.g.
// ArgAction::Set, it will now override itself.
//
// This is the equivalent to saying the `foo` arg using
// Arg::overrides_with("foo") for all defined arguments.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) ArgsOverrideSelf(yes bool) *Command {
	return c.setGlobalSetting(appSettings_AllArgsOverrideSelf, yes)
}

// DontDelimitTrailingValues is Command::dont_delimit_trailing_values:
// disables the automatic delimiting of values after `--` or when
// Arg::trailing_var_arg was used.
//
// NOTE: The same thing can be done manually by setting the final positional
// argument to Arg::value_delimiter(None). Using this setting is safer,
// because it's easier to locate when making changes.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) DontDelimitTrailingValues(yes bool) *Command {
	return c.setGlobalSetting(appSettings_DontDelimitTrailingValues, yes)
}

// Color is Command::color: sets when to color output.
//
// To customize how the output is styled, see Command::styles.
//
// NOTE: This choice is propagated to all child subcommands.
//
// NOTE: Default behaviour is `ColorChoice::Auto`.
func (c *Command) Color(color ColorChoice) *Command {
	cmd := c.
		unsetGlobalSetting(appSettings_ColorAuto).
		unsetGlobalSetting(appSettings_ColorAlways).
		unsetGlobalSetting(appSettings_ColorNever)
	switch color {
	case ColorChoice_Auto:
		return cmd.globalSetting(appSettings_ColorAuto)
	case ColorChoice_Always:
		return cmd.globalSetting(appSettings_ColorAlways)
	case ColorChoice_Never:
		return cmd.globalSetting(appSettings_ColorNever)
	default:
		panic(rs.Unreachable())
	}
}

// Styles is Command::styles: sets the Styles for terminal output.
//
// NOTE: This choice is propagated to all child subcommands.
//
// NOTE: Default behaviour is Styles::default.
func (c *Command) Styles(styles BuilderStyles) *Command {
	c.appExt.styles = rs.Some(styles)
	return c
}

// TermWidth is Command::term_width: sets the terminal width at which to
// wrap help messages.
//
// Using `0` will ignore terminal widths and use source formatting.
//
// Defaults to current terminal width when `wrap_help` feature flag is
// enabled. If current width cannot be determined, the default is 100.
//
// NOTE: This setting applies globally and *not* on a per-command basis.
//
// NOTE: This requires the `wrap_help` feature. Mjolnir builds clap without
// it: the width then only decides whether an argument's help moves to the
// next line.
func (c *Command) TermWidth(width int) *Command {
	c.appExt.termWidth = rs.Some(width)
	return c
}

// MaxTermWidth is Command::max_term_width: limit the line length for
// wrapping help when using the current terminal's width.
//
// This only applies when Command::term_width is unset so that the current
// terminal's width will be used. See Command::term_width for more details.
//
// Using `0` will ignore this, always respecting Command::term_width
// (default).
//
// NOTE: This setting applies globally and *not* on a per-command basis.
//
// NOTE: This requires the `wrap_help` feature
func (c *Command) MaxTermWidth(width int) *Command {
	c.appExt.maxTermWidth = rs.Some(width)
	return c
}

// DisableVersionFlag is Command::disable_version_flag: disables `-V` and
// `--version` flag.
func (c *Command) DisableVersionFlag(yes bool) *Command {
	return c.setGlobalSetting(appSettings_DisableVersionFlag, yes)
}

// PropagateVersion is Command::propagate_version: specifies to use the
// version of the current command for all subcommands.
//
// Defaults to `false`; subcommands have independent version strings from
// their parents.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) PropagateVersion(yes bool) *Command {
	return c.setGlobalSetting(appSettings_PropagateVersion, yes)
}

// NextLineHelp is Command::next_line_help: places the help string for all
// arguments and subcommands on the line after them.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) NextLineHelp(yes bool) *Command {
	return c.setGlobalSetting(appSettings_NextLineHelp, yes)
}

// DisableHelpFlag is Command::disable_help_flag: disables `-h` and `--help`
// flag.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) DisableHelpFlag(yes bool) *Command {
	return c.setGlobalSetting(appSettings_DisableHelpFlag, yes)
}

// DisableHelpSubcommand is Command::disable_help_subcommand: disables the
// `help` subcommand.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) DisableHelpSubcommand(yes bool) *Command {
	return c.setGlobalSetting(appSettings_DisableHelpSubcommand, yes)
}

// DisableColoredHelp is Command::disable_colored_help: disables colorized
// help messages.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) DisableColoredHelp(yes bool) *Command {
	return c.setGlobalSetting(appSettings_DisableColoredHelp, yes)
}

// HelpExpected is Command::help_expected: panic if help descriptions are
// omitted.
//
// NOTE: When deriving Parser, you could instead check this at compile-time
// with `#![deny(missing_docs)]`
//
// NOTE: This choice is propagated to all child subcommands.
//
// Panics on debug builds if any argument has no help.
func (c *Command) HelpExpected(yes bool) *Command {
	return c.setGlobalSetting(appSettings_HelpExpected, yes)
}

// HidePossibleValues is Command::hide_possible_values: tells `clap` *not*
// to print possible values when displaying help information.
//
// This can be useful if there are many values, or they are explained
// elsewhere.
//
// To set this per argument, see Arg::hide_possible_values.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) HidePossibleValues(yes bool) *Command {
	return c.setGlobalSetting(appSettings_HidePossibleValues, yes)
}

// InferLongArgs is Command::infer_long_args: allow partial matches of long
// arguments or their aliases.
//
// For example, to match an argument named `--test`, one could use `--t`,
// `--te`, `--tes`, and `--test`.
//
// NOTE: The match *must not* be ambiguous at all in order to succeed. i.e.
// to match `--te` to `--test` there could not also be another argument or
// alias `--temp` because both start with `--te`
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) InferLongArgs(yes bool) *Command {
	return c.setGlobalSetting(appSettings_InferLongArgs, yes)
}

// InferSubcommands is Command::infer_subcommands: allow partial matches of
// subcommand names and their aliases.
//
// For example, to match a subcommand named `test`, one could use `t`, `te`,
// `tes`, and `test`.
//
// NOTE: The match *must not* be ambiguous at all in order to succeed. i.e.
// to match `te` to `test` there could not also be a subcommand or alias
// `temp` because both start with `te`
//
// WARNING: This setting can interfere with positional/free arguments, take
// care when designing CLIs which allow inferred subcommands and have
// potential positional/free arguments whose values could start with the
// same characters as subcommands. If this is the case, it's recommended to
// use settings such as Command::args_conflicts_with_subcommands in
// conjunction with this setting.
//
// NOTE: This choice is propagated to all child subcommands.
func (c *Command) InferSubcommands(yes bool) *Command {
	return c.setGlobalSetting(appSettings_InferSubcommands, yes)
}

// # Command-specific Settings
//
// These apply only to the current command and are not inherited by
// subcommands.

// Name is Command::name: (re)sets the program's name.
//
// See Command::new for more details.
func (c *Command) Name(name string) *Command {
	c.name = name
	return c
}

// BinName is Command::bin_name: overrides the runtime-determined name of
// the binary for help and error messages.
//
// This should only be used when absolutely necessary, such as when the
// binary name for your application is misleading, or perhaps *not* how the
// user should invoke your program.
//
// TIP: When building things such as third party `cargo` subcommands, this
// setting **should** be used!
//
// NOTE: This *does not* change or set the name of the binary file on disk.
// It only changes what clap thinks the name is for the purposes of error or
// help messages.
func (c *Command) BinName(name string) *Command {
	c.binName = rs.Some(name)
	return c
}

// BinNameReset is Command::bin_name(None).
func (c *Command) BinNameReset() *Command {
	c.binName = rs.None[string]()
	return c
}

// DisplayName is Command::display_name: overrides the runtime-determined
// display name of the program for help and error messages.
func (c *Command) DisplayName(name string) *Command {
	c.displayName = rs.Some(name)
	return c
}

// DisplayNameReset is Command::display_name(None).
func (c *Command) DisplayNameReset() *Command {
	c.displayName = rs.None[string]()
	return c
}

// Author is Command::author: sets the author(s) for the help message.
//
// TIP: Use `clap`s convenience macro `crate_authors!` to automatically set
// your application's author(s) to the same thing as your crate at compile
// time.
//
// NOTE: A custom help_template is needed for author to show up.
func (c *Command) Author(author string) *Command {
	c.author = rs.Some(author)
	return c
}

// AuthorReset is Command::author(None).
func (c *Command) AuthorReset() *Command {
	c.author = rs.None[string]()
	return c
}

// About is Command::about: sets the program's description for the short
// help (`-h`).
//
// If Command::long_about is not specified, this message will be displayed
// for `--help`.
//
// NOTE: Only Command::about (short format) is used in completion script
// generation in order to be concise.
//
// See also `crate_description!`.
func (c *Command) About(about string) *Command {
	c.about = rs.Some(BuilderStyledStr(about))
	return c
}

// AboutReset is Command::about(None).
func (c *Command) AboutReset() *Command {
	c.about = rs.None[BuilderStyledStr]()
	return c
}

// LongAbout is Command::long_about: sets the program's description for the
// long help (`--help`).
//
// If not set, Command::about will be used for long help in addition to
// short help (`-h`).
//
// NOTE: Only Command::about (short format) is used in completion script
// generation in order to be concise.
func (c *Command) LongAbout(longAbout string) *Command {
	c.longAbout = rs.Some(BuilderStyledStr(longAbout))
	return c
}

// LongAboutReset is Command::long_about(None).
func (c *Command) LongAboutReset() *Command {
	c.longAbout = rs.None[BuilderStyledStr]()
	return c
}

// AfterHelp is Command::after_help: free-form help text for after
// auto-generated short help (`-h`).
//
// This is often used to describe how to use the arguments, caveats to be
// noted, or license and contact information.
//
// If Command::after_long_help is not specified, this message will be
// displayed for `--help`.
func (c *Command) AfterHelp(help string) *Command {
	c.afterHelp = rs.Some(BuilderStyledStr(help))
	return c
}

// AfterHelpReset is Command::after_help(None).
func (c *Command) AfterHelpReset() *Command {
	c.afterHelp = rs.None[BuilderStyledStr]()
	return c
}

// AfterLongHelp is Command::after_long_help: free-form help text for after
// auto-generated long help (`--help`).
//
// This is often used to describe how to use the arguments, caveats to be
// noted, or license and contact information.
//
// If not set, Command::after_help will be used for long help in addition to
// short help (`-h`).
func (c *Command) AfterLongHelp(help string) *Command {
	c.afterLongHelp = rs.Some(BuilderStyledStr(help))
	return c
}

// AfterLongHelpReset is Command::after_long_help(None).
func (c *Command) AfterLongHelpReset() *Command {
	c.afterLongHelp = rs.None[BuilderStyledStr]()
	return c
}

// BeforeHelp is Command::before_help: free-form help text for before
// auto-generated short help (`-h`).
//
// This is often used for header, copyright, or license information.
//
// If Command::before_long_help is not specified, this message will be
// displayed for `--help`.
func (c *Command) BeforeHelp(help string) *Command {
	c.beforeHelp = rs.Some(BuilderStyledStr(help))
	return c
}

// BeforeHelpReset is Command::before_help(None).
func (c *Command) BeforeHelpReset() *Command {
	c.beforeHelp = rs.None[BuilderStyledStr]()
	return c
}

// BeforeLongHelp is Command::before_long_help: free-form help text for
// before auto-generated long help (`--help`).
//
// This is often used for header, copyright, or license information.
//
// If not set, Command::before_help will be used for long help in addition
// to short help (`-h`).
func (c *Command) BeforeLongHelp(help string) *Command {
	c.beforeLongHelp = rs.Some(BuilderStyledStr(help))
	return c
}

// BeforeLongHelpReset is Command::before_long_help(None).
func (c *Command) BeforeLongHelpReset() *Command {
	c.beforeLongHelp = rs.None[BuilderStyledStr]()
	return c
}

// Version is Command::version: sets the version for the short version
// (`-V`) and help messages.
//
// If Command::long_version is not specified, this message will be displayed
// for `--version`.
//
// TIP: Use `clap`s convenience macro `crate_version!` to automatically set
// your application's version to the same thing as your crate at compile
// time.
func (c *Command) Version(ver string) *Command {
	c.version = rs.Some(ver)
	return c
}

// VersionReset is Command::version(None).
func (c *Command) VersionReset() *Command {
	c.version = rs.None[string]()
	return c
}

// LongVersion is Command::long_version: sets the version for the long
// version (`--version`) and help messages.
//
// If Command::version is not specified, this message will be displayed for
// `-V`.
//
// TIP: Use `clap`s convenience macro `crate_version!` to automatically set
// your application's version to the same thing as your crate at compile
// time.
func (c *Command) LongVersion(ver string) *Command {
	c.longVersion = rs.Some(ver)
	return c
}

// LongVersionReset is Command::long_version(None).
func (c *Command) LongVersionReset() *Command {
	c.longVersion = rs.None[string]()
	return c
}

// OverrideUsage is Command::override_usage: overrides the `clap` generated
// usage string for help and error messages.
//
// NOTE: Using this setting disables `clap`s "context-aware" usage strings.
// After this setting is set, this will be *the only* usage string
// displayed to the user!
//
// NOTE: Multiple usage lines may be present in the usage argument, but some
// rules need to be followed to ensure the usage lines are formatted
// correctly by the default help formatter:
//
//   - Do not indent the first usage line.
//   - Indent all subsequent usage lines with seven spaces.
//   - The last line must not end with a newline.
func (c *Command) OverrideUsage(usage string) *Command {
	c.usageStr = rs.Some(BuilderStyledStr(usage))
	return c
}

// OverrideUsageReset is Command::override_usage(None).
func (c *Command) OverrideUsageReset() *Command {
	c.usageStr = rs.None[BuilderStyledStr]()
	return c
}

// OverrideHelp is Command::override_help: overrides the `clap` generated
// help message (both `-h` and `--help`).
//
// This should only be used when the auto-generated message does not
// suffice.
//
// NOTE: This **only** replaces the help message for the current command,
// meaning if you are using subcommands, those help messages will still be
// auto-generated unless you specify a Command::override_help for them as
// well.
func (c *Command) OverrideHelp(help string) *Command {
	c.helpStr = rs.Some(BuilderStyledStr(help))
	return c
}

// OverrideHelpReset is Command::override_help(None).
func (c *Command) OverrideHelpReset() *Command {
	c.helpStr = rs.None[BuilderStyledStr]()
	return c
}

// HelpTemplate is Command::help_template: sets the help template to be
// used, overriding the default format.
//
// Tags are given inside curly brackets.
//
// Valid tags are:
//
//   - `{name}`                - Display name for the (sub-)command.
//   - `{bin}`                 - Binary name.(deprecated)
//   - `{version}`             - Version number.
//   - `{author}`              - Author information.
//   - `{author-with-newline}` - Author followed by `\n`.
//   - `{author-section}`      - Author preceded and followed by `\n`.
//   - `{about}`               - General description (from Command::about or
//     Command::long_about).
//   - `{about-with-newline}`  - About followed by `\n`.
//   - `{about-section}`       - About preceded and followed by '\n'.
//   - `{usage-heading}`       - Automatically generated usage heading.
//   - `{usage}`               - Automatically generated or given usage
//     string.
//   - `{all-args}`            - Help for all arguments (options, flags,
//     positional arguments, and subcommands) including titles.
//   - `{options}`             - Help for options.
//   - `{positionals}`         - Help for positional arguments.
//   - `{subcommands}`         - Help for subcommands.
//   - `{tab}`                 - Standard tab sized used within clap
//   - `{after-help}`          - Help from Command::after_help or
//     Command::after_long_help.
//   - `{before-help}`         - Help from Command::before_help or
//     Command::before_long_help.
func (c *Command) HelpTemplate(s string) *Command {
	c.template = rs.Some(BuilderStyledStr(s))
	return c
}

// HelpTemplateReset is Command::help_template(None).
func (c *Command) HelpTemplateReset() *Command {
	c.template = rs.None[BuilderStyledStr]()
	return c
}

func (c *Command) setting(setting appSettings) *Command {
	c.settings.set(setting)
	return c
}

func (c *Command) unsetSetting(setting appSettings) *Command {
	c.settings.unset(setting)
	return c
}

func (c *Command) setSetting(setting appSettings, yes bool) *Command {
	if yes {
		return c.setting(setting)
	}
	return c.unsetSetting(setting)
}

func (c *Command) globalSetting(setting appSettings) *Command {
	c.settings.set(setting)
	c.gSettings.set(setting)
	return c
}

func (c *Command) unsetGlobalSetting(setting appSettings) *Command {
	c.settings.unset(setting)
	c.gSettings.unset(setting)
	return c
}

func (c *Command) setGlobalSetting(setting appSettings, yes bool) *Command {
	if yes {
		return c.globalSetting(setting)
	}
	return c.unsetGlobalSetting(setting)
}

// FlattenHelp is Command::flatten_help: flatten subcommand help into the
// current command's help.
//
// This shows a summary of subcommands within the usage and help for the
// current command, similar to `git stash --help` showing information on
// `push`, `pop`, etc. To see more information, a user can still pass
// `--help` to the individual subcommands.
func (c *Command) FlattenHelp(yes bool) *Command {
	return c.setSetting(appSettings_FlattenHelp, yes)
}

// NextHelpHeading is Command::next_help_heading: set the default section
// heading for future args.
//
// This will be used for any arg that hasn't had Arg::help_heading called.
//
// This is useful if the default `Options` or `Arguments` headings are not
// specific enough for one's use case.
//
// For subcommands, see Command::subcommand_help_heading
func (c *Command) NextHelpHeading(heading string) *Command {
	c.currentHelpHeading = rs.Some(heading)
	return c
}

// NextHelpHeadingReset is Command::next_help_heading(None).
func (c *Command) NextHelpHeadingReset() *Command {
	c.currentHelpHeading = rs.None[string]()
	return c
}

// NextDisplayOrder is Command::next_display_order: change the starting
// value for assigning future display orders for args.
//
// This will be used for any arg that hasn't had Arg::display_order called.
func (c *Command) NextDisplayOrder(dispOrd int) *Command {
	c.currentDispOrd = rs.Some(dispOrd)
	return c
}

// NextDisplayOrderReset is Command::next_display_order(None).
func (c *Command) NextDisplayOrderReset() *Command {
	c.currentDispOrd = rs.None[int]()
	return c
}

// ArgRequiredElseHelp is Command::arg_required_else_help: exit gracefully
// if no arguments are present (e.g. `$ myprog`).
//
// NOTE: subcommands count as arguments
func (c *Command) ArgRequiredElseHelp(yes bool) *Command {
	return c.setSetting(appSettings_ArgRequiredElseHelp, yes)
}

// AllowMissingPositional is Command::allow_missing_positional: allows one
// to implement two styles of CLIs where positionals can be used out of
// order.
//
// The first example is a CLI where the second to last positional argument
// is optional, but the final positional argument is required. Such as
// `$ prog [optional] <required>` where one of the two following usages is
// allowed:
//
//   - `$ prog [optional] <required>`
//   - `$ prog <required>`
//
// This would otherwise not be allowed. This is useful when `[optional]` has
// a default value.
//
// Note: when using this style of "missing positionals" the final positional
// *must* be required when `--` will not be used to skip to the final
// positional argument.
//
// Note: This style also only allows a single positional argument to be
// "skipped" without the use of `--`. To skip more than one, see the second
// example.
//
// The second example is when one wants to skip multiple optional positional
// arguments, and use of the `--` operator is OK (but not required if all
// arguments will be specified anyways).
//
// For example, imagine a CLI which has three positional arguments `[foo]
// [bar] [baz]...` where `baz` accepts multiple values (similar to man
// `ARGS...` style training arguments).
//
// With this setting the following invocations are possible:
//
//   - `$ prog foo bar baz1 baz2 baz3`
//   - `$ prog foo -- baz1 baz2 baz3`
//   - `$ prog -- baz1 baz2 baz3`
func (c *Command) AllowMissingPositional(yes bool) *Command {
	return c.setSetting(appSettings_AllowMissingPositional, yes)
}

// # Subcommand-specific Settings

// ShortFlag is Command::short_flag: sets the short version of the
// subcommand flag without the preceding `-`.
//
// Allows the subcommand to be used as if it were an Arg::short.
func (c *Command) ShortFlag(short rune) *Command {
	c.shortFlag = rs.Some(short)
	return c
}

// ShortFlagReset is Command::short_flag(None).
func (c *Command) ShortFlagReset() *Command {
	c.shortFlag = rs.None[rune]()
	return c
}

// LongFlag is Command::long_flag: sets the long version of the subcommand
// flag without the preceding `--`.
//
// Allows the subcommand to be used as if it were an Arg::long.
//
// NOTE: Any leading `-` characters will be stripped.
func (c *Command) LongFlag(long string) *Command {
	c.longFlag = rs.Some(long)
	return c
}

// Alias is Command::alias: sets a hidden alias to this subcommand.
//
// This allows the subcommand to be accessed via *either* the original name,
// or this given alias. This is more efficient and easier than creating
// multiple hidden subcommands as one only needs to check for the existence
// of this command, and not all aliased variants.
//
// NOTE: Aliases defined with this method are *hidden* from the help
// message. If you're looking for aliases that will be displayed in the help
// message, see Command::visible_alias.
//
// NOTE: When using aliases and checking for the existence of a particular
// subcommand within an ArgMatches struct, one only needs to search for the
// original name and not all aliases.
func (c *Command) Alias(name string) *Command {
	c.aliases = append(c.aliases, commandAlias{name: name, visible: false})
	return c
}

// AliasReset is Command::alias(None): removes the aliases.
func (c *Command) AliasReset() *Command {
	c.aliases = nil
	return c
}

// ShortFlagAlias is Command::short_flag_alias: add an alias, which
// functions as "hidden" short flag subcommand
//
// This will automatically dispatch as if this subcommand was used. This is
// more efficient, and easier than creating multiple hidden subcommands as
// one only needs to check for the existence of this command, and not all
// variants.
func (c *Command) ShortFlagAlias(name rune) *Command {
	if debugAssertions && name == '-' {
		rs.PanicAt(1, "short alias name cannot be `-`")
	}
	c.shortFlagAliases = append(c.shortFlagAliases, commandShortAlias{name: name, visible: false})
	return c
}

// ShortFlagAliasReset is Command::short_flag_alias(None).
func (c *Command) ShortFlagAliasReset() *Command {
	c.shortFlagAliases = nil
	return c
}

// LongFlagAlias is Command::long_flag_alias: add an alias, which functions
// as a "hidden" long flag subcommand.
//
// This will automatically dispatch as if this subcommand was used. This is
// more efficient, and easier than creating multiple hidden subcommands as
// one only needs to check for the existence of this command, and not all
// variants.
func (c *Command) LongFlagAlias(name string) *Command {
	c.longFlagAliases = append(c.longFlagAliases, commandAlias{name: name, visible: false})
	return c
}

// LongFlagAliasReset is Command::long_flag_alias(None).
func (c *Command) LongFlagAliasReset() *Command {
	c.longFlagAliases = nil
	return c
}

// Aliases is Command::aliases: sets multiple hidden aliases to this
// subcommand.
//
// This allows the subcommand to be accessed via *either* the original name
// or any of the given aliases. This is more efficient, and easier than
// creating multiple hidden subcommands as one only needs to check for the
// existence of this command and not all aliased variants.
//
// NOTE: Aliases defined with this method are *hidden* from the help
// message. If looking for aliases that will be displayed in the help
// message, see Command::visible_aliases.
//
// NOTE: When using aliases and checking for the existence of a particular
// subcommand within an ArgMatches struct, one only needs to search for the
// original name and not all aliases.
func (c *Command) Aliases(names ...string) *Command {
	for _, n := range names {
		c.aliases = append(c.aliases, commandAlias{name: n, visible: false})
	}
	return c
}

// ShortFlagAliases is Command::short_flag_aliases: add aliases, which
// function as "hidden" short flag subcommands.
//
// These will automatically dispatch as if this subcommand was used. This is
// more efficient, and easier than creating multiple hidden subcommands as
// one only needs to check for the existence of this command, and not all
// variants.
func (c *Command) ShortFlagAliases(names ...rune) *Command {
	for _, s := range names {
		if debugAssertions && s == '-' {
			rs.PanicAt(1, "short alias name cannot be `-`")
		}
		c.shortFlagAliases = append(c.shortFlagAliases, commandShortAlias{name: s, visible: false})
	}
	return c
}

// LongFlagAliases is Command::long_flag_aliases: add aliases, which
// function as "hidden" long flag subcommands.
//
// These will automatically dispatch as if this subcommand was used. This is
// more efficient, and easier than creating multiple hidden subcommands as
// one only needs to check for the existence of this command, and not all
// variants.
func (c *Command) LongFlagAliases(names ...string) *Command {
	for _, s := range names {
		c = c.LongFlagAlias(s)
	}
	return c
}

// VisibleAlias is Command::visible_alias: sets a visible alias to this
// subcommand.
//
// This allows the subcommand to be accessed via *either* the original name
// or the given alias. This is more efficient and easier than creating
// hidden subcommands as one only needs to check for the existence of this
// command and not all aliased variants.
//
// NOTE: The alias defined with this method is *visible* from the help
// message and displayed as if it were just another regular subcommand. If
// looking for an alias that will not be displayed in the help message, see
// Command::alias.
//
// NOTE: When using aliases and checking for the existence of a particular
// subcommand within an ArgMatches struct, one only needs to search for the
// original name and not all aliases.
func (c *Command) VisibleAlias(name string) *Command {
	c.aliases = append(c.aliases, commandAlias{name: name, visible: true})
	return c
}

// VisibleAliasReset is Command::visible_alias(None).
func (c *Command) VisibleAliasReset() *Command {
	c.aliases = nil
	return c
}

// VisibleShortFlagAlias is Command::visible_short_flag_alias: add an alias,
// which functions as "visible" short flag subcommand
//
// This will automatically dispatch as if this subcommand was used. This is
// more efficient, and easier than creating multiple hidden subcommands as
// one only needs to check for the existence of this command, and not all
// variants.
//
// See also Command::short_flag_alias.
func (c *Command) VisibleShortFlagAlias(name rune) *Command {
	if debugAssertions && name == '-' {
		rs.PanicAt(1, "short alias name cannot be `-`")
	}
	c.shortFlagAliases = append(c.shortFlagAliases, commandShortAlias{name: name, visible: true})
	return c
}

// VisibleShortFlagAliasReset is Command::visible_short_flag_alias(None).
func (c *Command) VisibleShortFlagAliasReset() *Command {
	c.shortFlagAliases = nil
	return c
}

// VisibleLongFlagAlias is Command::visible_long_flag_alias: add an alias,
// which functions as a "visible" long flag subcommand.
//
// This will automatically dispatch as if this subcommand was used. This is
// more efficient, and easier than creating multiple hidden subcommands as
// one only needs to check for the existence of this command, and not all
// variants.
//
// See also Command::long_flag_alias.
func (c *Command) VisibleLongFlagAlias(name string) *Command {
	c.longFlagAliases = append(c.longFlagAliases, commandAlias{name: name, visible: true})
	return c
}

// VisibleLongFlagAliasReset is Command::visible_long_flag_alias(None).
func (c *Command) VisibleLongFlagAliasReset() *Command {
	c.longFlagAliases = nil
	return c
}

// VisibleAliases is Command::visible_aliases: sets multiple visible aliases
// to this subcommand.
//
// This allows the subcommand to be accessed via *either* the original name
// or any of the given aliases. This is more efficient and easier than
// creating multiple hidden subcommands as one only needs to check for the
// existence of this command and not all aliased variants.
//
// NOTE: The alias defined with this method is *visible* from the help
// message and displayed as if it were just another regular subcommand. If
// looking for an alias that will not be displayed in the help message, see
// Command::alias.
//
// NOTE: When using aliases, and checking for the existence of a particular
// subcommand within an ArgMatches struct, one only needs to search for the
// original name and not all aliases.
func (c *Command) VisibleAliases(names ...string) *Command {
	for _, n := range names {
		c.aliases = append(c.aliases, commandAlias{name: n, visible: true})
	}
	return c
}

// VisibleShortFlagAliases is Command::visible_short_flag_aliases: add
// aliases, which function as *visible* short flag subcommands.
//
// See Command::short_flag_aliases.
func (c *Command) VisibleShortFlagAliases(names ...rune) *Command {
	for _, s := range names {
		if debugAssertions && s == '-' {
			rs.PanicAt(1, "short alias name cannot be `-`")
		}
		c.shortFlagAliases = append(c.shortFlagAliases, commandShortAlias{name: s, visible: true})
	}
	return c
}

// VisibleLongFlagAliases is Command::visible_long_flag_aliases: add
// aliases, which function as *visible* long flag subcommands.
//
// See Command::long_flag_aliases.
func (c *Command) VisibleLongFlagAliases(names ...string) *Command {
	for _, s := range names {
		c = c.VisibleLongFlagAlias(s)
	}
	return c
}

// DisplayOrder is Command::display_order: set the placement of this
// subcommand within the help.
//
// Subcommands with a lower value will be displayed first in the help
// message. Those with the same display order will be sorted.
//
// `Command`s are automatically assigned a display order based on the order
// they are added to their parent Command. Overriding this is helpful when
// the order commands are added in isn't the same as the display order,
// whether in one-off cases or to automatically sort commands.
func (c *Command) DisplayOrder(ord int) *Command {
	c.dispOrd = rs.Some(ord)
	return c
}

// DisplayOrderReset is Command::display_order(None).
func (c *Command) DisplayOrderReset() *Command {
	c.dispOrd = rs.None[int]()
	return c
}

// Hide is Command::hide: specifies that this subcommand should be hidden
// from help messages
func (c *Command) Hide(yes bool) *Command {
	return c.setSetting(appSettings_Hidden, yes)
}

// SubcommandRequired is Command::subcommand_required: if no subcommand is
// present at runtime, error and exit gracefully.
func (c *Command) SubcommandRequired(yes bool) *Command {
	return c.setSetting(appSettings_SubcommandRequired, yes)
}

// AllowExternalSubcommands is Command::allow_external_subcommands: assume
// unexpected positional arguments are a subcommand.
//
// Arguments will be stored in the `""` argument in the ArgMatches
//
// NOTE: Use this setting with caution, as a truly unexpected argument (i.e.
// one which is *NOT* an external subcommand) will **not** cause an error
// and instead be treated as a potential subcommand. One should check for
// such cases manually and inform the user appropriately.
//
// NOTE: A built-in subcommand will be parsed as an external subcommand when
// escaped with `--`.
func (c *Command) AllowExternalSubcommands(yes bool) *Command {
	return c.setSetting(appSettings_AllowExternalSubcommands, yes)
}

// ExternalSubcommandValueParser is
// Command::external_subcommand_value_parser: specifies how to parse
// external subcommand arguments.
//
// The default parser is for `OsString`. This can be used to switch it to
// `String` or another type.
//
// NOTE: Setting this requires Command::allow_external_subcommands
func (c *Command) ExternalSubcommandValueParser(parser BuilderIntoValueParser) *Command {
	c.externalValueParser = rs.Some(parser.IntoValueParser())
	return c
}

// ExternalSubcommandValueParserReset is
// Command::external_subcommand_value_parser(None).
func (c *Command) ExternalSubcommandValueParserReset() *Command {
	c.externalValueParser = rs.None[BuilderValueParser]()
	return c
}

// ArgsConflictsWithSubcommands is Command::args_conflicts_with_subcommands:
// specifies that use of an argument prevents the use of subcommands.
//
// By default `clap` allows arguments between subcommands such as
// `<cmd> [cmd_args] <subcmd> [subcmd_args] <subsubcmd> [subsubcmd_args]`.
//
// This setting disables that functionality and says that arguments can only
// follow the *final* subcommand. For instance using this setting makes only
// the following invocations possible:
//
//   - `<cmd> <subcmd> <subsubcmd> [subsubcmd_args]`
//   - `<cmd> <subcmd> [subcmd_args]`
//   - `<cmd> [cmd_args]`
func (c *Command) ArgsConflictsWithSubcommands(yes bool) *Command {
	return c.setSetting(appSettings_ArgsNegateSubcommands, yes)
}

// SubcommandPrecedenceOverArg is Command::subcommand_precedence_over_arg:
// prevent subcommands from being consumed as an arguments value.
//
// By default, if an option taking multiple values is followed by a
// subcommand, the subcommand will be parsed as another value.
//
//	cmd --foo val1 val2 subcommand
//	          --------- ----------
//	            values   another value
//
// This setting instructs the parser to stop when encountering a subcommand
// instead of greedily consuming arguments.
//
//	cmd --foo val1 val2 subcommand
//	          --------- ----------
//	            values   subcommand
func (c *Command) SubcommandPrecedenceOverArg(yes bool) *Command {
	return c.setSetting(appSettings_SubcommandPrecedenceOverArg, yes)
}

// SubcommandNegatesReqs is Command::subcommand_negates_reqs: allows
// subcommands to override all requirements of the parent command.
//
// For example, if you had a subcommand or top level application with a
// required argument that is only required as long as there is no
// subcommand present, using this setting would allow you to set those
// arguments to Arg::required(true) and yet receive no error so long as the
// user uses a valid subcommand instead.
//
// NOTE: This defaults to false (using subcommand does *not* negate
// requirements)
func (c *Command) SubcommandNegatesReqs(yes bool) *Command {
	return c.setSetting(appSettings_SubcommandsNegateReqs, yes)
}

// Multicall is Command::multicall: multiple-personality program dispatched
// on the binary name (`argv[0]`)
//
// A "multicall" executable is a single executable that contains a variety
// of applets, and decides which applet to run based on the name of the
// file. The executable can be called from different names by creating hard
// links or symbolic links to it.
//
// This is desirable for:
//   - Easy distribution, a single binary that can install hardlinks to
//     access the different personalities.
//   - Minimal binary size by sharing common code (e.g. standard library,
//     clap)
//   - Custom shells or REPLs where there isn't a single top-level command
//
// Setting `multicall` will cause
//   - `argv[0]` to be stripped to the base name and parsed as the first
//     argument, as if Command::no_binary_name was set.
//   - Help and errors to report subcommands as if they were the top-level
//     command
//
// When the subcommand is not present, there are several strategies you may
// employ, depending on your needs:
//   - Let the error percolate up normally
//   - Print a specialized error message using the Error::context
//   - Print the help but this might be ambiguous
//   - Disable `multicall` and re-parse it
//   - Disable `multicall` and re-parse it with a specific subcommand
//
// When detecting the error condition, the ErrorKind isn't sufficient as a
// sub-subcommand might report the same error. Enable
// allow_external_subcommands if you want to specifically get the
// unrecognized binary name.
//
// NOTE: Multicall can't be used with no_binary_name since they interpret
// the command name in incompatible ways.
//
// NOTE: The multicall command cannot have arguments.
//
// NOTE: Applets are slightly semantically different from subcommands, so
// it's recommended to use Command::subcommand_help_heading and
// Command::subcommand_value_name to change the descriptive text as above.
func (c *Command) Multicall(yes bool) *Command {
	return c.setSetting(appSettings_Multicall, yes)
}

// SubcommandValueName is Command::subcommand_value_name: sets the value
// name used for subcommands when printing usage and help.
//
// By default, this is "COMMAND".
//
// See also Command::subcommand_help_heading
func (c *Command) SubcommandValueName(valueName string) *Command {
	c.subcommandValueName = rs.Some(valueName)
	return c
}

// SubcommandValueNameReset is Command::subcommand_value_name(None).
func (c *Command) SubcommandValueNameReset() *Command {
	c.subcommandValueName = rs.None[string]()
	return c
}

// SubcommandHelpHeading is Command::subcommand_help_heading: sets the help
// heading used for subcommands when printing usage and help.
//
// By default, this is "Commands".
//
// See also Command::subcommand_value_name
func (c *Command) SubcommandHelpHeading(heading string) *Command {
	c.subcommandHeading = rs.Some(heading)
	return c
}

// SubcommandHelpHeadingReset is Command::subcommand_help_heading(None).
func (c *Command) SubcommandHelpHeadingReset() *Command {
	c.subcommandHeading = rs.None[string]()
	return c
}

// # Reflection

func (c *Command) getUsageName() rs.Option[string] {
	return c.usageName
}

func (c *Command) getUsageNameFallback() string {
	if name, ok := c.getUsageName().Get(); ok {
		return name
	}
	return c.getBinNameFallback()
}

// GetDisplayName is Command::get_display_name: get the name of the binary.
func (c *Command) GetDisplayName() rs.Option[string] {
	return c.displayName
}

// GetBinName is Command::get_bin_name: get the name of the binary.
func (c *Command) GetBinName() rs.Option[string] {
	return c.binName
}

// getBinNameFallback gets the name of the binary.
func (c *Command) getBinNameFallback() string {
	if name, ok := c.binName.Get(); ok {
		return name
	}
	return c.GetName()
}

// SetBinName is Command::set_bin_name: set binary name. Uses `&mut self`
// instead of `self`.
func (c *Command) SetBinName(name string) {
	c.binName = rs.Some(name)
}

// GetName is Command::get_name: get the name of the cmd.
func (c *Command) GetName() string {
	return c.name
}

// GetNameAndVisibleAliases is Command::get_name_and_visible_aliases: get
// all known names of the cmd (i.e. primary name and visible aliases).
func (c *Command) GetNameAndVisibleAliases() []string {
	names := []string{c.name}
	names = append(names, c.GetVisibleAliases()...)
	return names
}

// GetVersion is Command::get_version: get the version of the cmd.
func (c *Command) GetVersion() rs.Option[string] {
	return c.version
}

// GetLongVersion is Command::get_long_version: get the long version of the
// cmd.
func (c *Command) GetLongVersion() rs.Option[string] {
	return c.longVersion
}

// GetDisplayOrder is Command::get_display_order: get the placement within
// help.
func (c *Command) GetDisplayOrder() int {
	return c.dispOrd.UnwrapOr(999)
}

// GetAuthor is Command::get_author: get the authors of the cmd.
func (c *Command) GetAuthor() rs.Option[string] {
	return c.author
}

// GetShortFlag is Command::get_short_flag: get the short flag of the
// subcommand.
func (c *Command) GetShortFlag() rs.Option[rune] {
	return c.shortFlag
}

// GetLongFlag is Command::get_long_flag: get the long flag of the
// subcommand.
func (c *Command) GetLongFlag() rs.Option[string] {
	return c.longFlag
}

// GetAbout is Command::get_about: get the help message specified via
// Command::about.
func (c *Command) GetAbout() rs.Option[BuilderStyledStr] {
	return c.about
}

// GetLongAbout is Command::get_long_about: get the help message specified
// via Command::long_about.
func (c *Command) GetLongAbout() rs.Option[BuilderStyledStr] {
	return c.longAbout
}

// IsFlattenHelpSet is Command::is_flatten_help_set: get the custom section
// heading specified via Command::flatten_help.
func (c *Command) IsFlattenHelpSet() bool {
	return c.isSet(appSettings_FlattenHelp)
}

// GetNextHelpHeading is Command::get_next_help_heading: get the custom
// section heading specified via Command::next_help_heading.
func (c *Command) GetNextHelpHeading() rs.Option[string] {
	return c.currentHelpHeading
}

// GetVisibleAliases is Command::get_visible_aliases: iterate through the
// *visible* aliases for this subcommand.
func (c *Command) GetVisibleAliases() []string {
	var names []string
	for _, alias := range c.aliases {
		if alias.visible {
			names = append(names, alias.name)
		}
	}
	return names
}

// GetVisibleShortFlagAliases is Command::get_visible_short_flag_aliases:
// iterate through the *visible* short aliases for this subcommand.
func (c *Command) GetVisibleShortFlagAliases() []rune {
	var names []rune
	for _, alias := range c.shortFlagAliases {
		if alias.visible {
			names = append(names, alias.name)
		}
	}
	return names
}

// GetVisibleLongFlagAliases is Command::get_visible_long_flag_aliases:
// iterate through the *visible* long aliases for this subcommand.
func (c *Command) GetVisibleLongFlagAliases() []string {
	var names []string
	for _, alias := range c.longFlagAliases {
		if alias.visible {
			names = append(names, alias.name)
		}
	}
	return names
}

// GetAllAliases is Command::get_all_aliases: iterate through the set of
// *all* the aliases for this subcommand, both visible and hidden.
func (c *Command) GetAllAliases() []string {
	names := make([]string, len(c.aliases))
	for i, alias := range c.aliases {
		names[i] = alias.name
	}
	return names
}

// GetAllShortFlagAliases is Command::get_all_short_flag_aliases: iterate
// through the set of *all* the short aliases for this subcommand, both
// visible and hidden.
func (c *Command) GetAllShortFlagAliases() []rune {
	names := make([]rune, len(c.shortFlagAliases))
	for i, alias := range c.shortFlagAliases {
		names[i] = alias.name
	}
	return names
}

// GetAllLongFlagAliases is Command::get_all_long_flag_aliases: iterate
// through the set of *all* the long aliases for this subcommand, both
// visible and hidden.
func (c *Command) GetAllLongFlagAliases() []string {
	names := make([]string, len(c.longFlagAliases))
	for i, alias := range c.longFlagAliases {
		names[i] = alias.name
	}
	return names
}

// GetAliases is Command::get_aliases: iterate through the *hidden* aliases
// for this subcommand.
func (c *Command) GetAliases() []string {
	var names []string
	for _, alias := range c.aliases {
		if !alias.visible {
			names = append(names, alias.name)
		}
	}
	return names
}

func (c *Command) isSet(s appSettings) bool {
	return c.settings.isSet(s) || c.gSettings.isSet(s)
}

// GetColor is Command::get_color: should we color the output?
func (c *Command) GetColor() ColorChoice {
	if c.isSet(appSettings_ColorNever) {
		return ColorChoice_Never
	} else if c.isSet(appSettings_ColorAlways) {
		return ColorChoice_Always
	}
	return ColorChoice_Auto
}

// GetStyles is Command::get_styles: return the current Styles for the
// Command.
func (c *Command) GetStyles() *BuilderStyles {
	if styles := c.appExt.styles.AsRef(); styles != nil {
		return styles
	}
	return &stylesDefault
}

// GetSubcommands is Command::get_subcommands: iterate through the set of
// subcommands, getting a reference to each.
func (c *Command) GetSubcommands() []*Command {
	return c.subcommands
}

// GetSubcommandsMut is Command::get_subcommands_mut: iterate through the
// set of subcommands, getting a mutable reference to each.
func (c *Command) GetSubcommandsMut() []*Command {
	return c.subcommands
}

// HasSubcommands is Command::has_subcommands: returns `true` if this
// `Command` has subcommands.
func (c *Command) HasSubcommands() bool {
	return len(c.subcommands) != 0
}

// GetSubcommandHelpHeading is Command::get_subcommand_help_heading: returns
// the help heading for listing subcommands.
func (c *Command) GetSubcommandHelpHeading() rs.Option[string] {
	return c.subcommandHeading
}

// GetSubcommandValueName is Command::get_subcommand_value_name: returns the
// subcommand value name.
func (c *Command) GetSubcommandValueName() rs.Option[string] {
	return c.subcommandValueName
}

// GetBeforeHelp is Command::get_before_help: returns the help heading for
// listing subcommands.
func (c *Command) GetBeforeHelp() rs.Option[BuilderStyledStr] {
	return c.beforeHelp
}

// GetBeforeLongHelp is Command::get_before_long_help: returns the help
// heading for listing subcommands.
func (c *Command) GetBeforeLongHelp() rs.Option[BuilderStyledStr] {
	return c.beforeLongHelp
}

// GetAfterHelp is Command::get_after_help: returns the help heading for
// listing subcommands.
func (c *Command) GetAfterHelp() rs.Option[BuilderStyledStr] {
	return c.afterHelp
}

// GetAfterLongHelp is Command::get_after_long_help: returns the help
// heading for listing subcommands.
func (c *Command) GetAfterLongHelp() rs.Option[BuilderStyledStr] {
	return c.afterLongHelp
}

// FindSubcommand is Command::find_subcommand: find subcommand such that its
// name or one of aliases equals `name`.
//
// This does not recurse through subcommands of subcommands.
func (c *Command) FindSubcommand(name string) rs.Option[*Command] {
	if sc := c.findSubcommand(name); sc != nil {
		return rs.Some(sc)
	}
	return rs.None[*Command]()
}

func (c *Command) findSubcommand(name string) *Command {
	for _, s := range c.subcommands {
		if s.aliasesTo(name) {
			return s
		}
	}
	return nil
}

// FindSubcommandMut is Command::find_subcommand_mut: find subcommand such
// that its name or one of aliases equals `name`, returning a mutable
// reference to the subcommand.
//
// This does not recurse through subcommands of subcommands.
func (c *Command) FindSubcommandMut(name string) rs.Option[*Command] {
	return c.FindSubcommand(name)
}

// GetGroups is Command::get_groups: iterate through the set of groups.
func (c *Command) GetGroups() []*ArgGroup {
	return c.groups
}

// GetArguments is Command::get_arguments: iterate through the set of
// arguments.
func (c *Command) GetArguments() []*Arg {
	return c.args.args
}

// GetPositionals is Command::get_positionals: iterate through the
// *positionals* arguments.
func (c *Command) GetPositionals() []*Arg {
	var positionals []*Arg
	for _, a := range c.args.args {
		if a.IsPositional() {
			positionals = append(positionals, a)
		}
	}
	return positionals
}

// GetOpts is Command::get_opts: iterate through the *options*.
func (c *Command) GetOpts() []*Arg {
	var opts []*Arg
	for _, a := range c.args.args {
		if a.isTakesValueSet() && !a.IsPositional() {
			opts = append(opts, a)
		}
	}
	return opts
}

// GetArgConflictsWith is Command::get_arg_conflicts_with: get a list of all
// arguments the given argument conflicts with.
//
// If the provided argument is declared as global, the conflicts will be
// determined based on the propagation rules of global arguments.
//
// Panics if the given arg contains a conflict with an argument that is
// unknown to this `Command`.
func (c *Command) GetArgConflictsWith(arg *Arg) []*Arg {
	if arg.IsGlobalSet() {
		return c.getGlobalArgConflictsWith(arg)
	}
	result := []*Arg{}
	for _, id := range arg.blacklist {
		if found := c.find(id); found != nil {
			result = append(result, found)
		} else if group := c.findGroup(id); group != nil {
			for _, member := range c.unrollArgsInGroup(group.id) {
				found := c.find(member)
				if found == nil {
					rs.PanicAt(1, internalErrorMsg)
				}
				result = append(result, found)
			}
		} else {
			rs.PanicAt(1, "Command::get_arg_conflicts_with: The passed arg conflicts with an arg unknown to the cmd")
		}
	}
	return result
}

// getGlobalArgConflictsWith gets a unique list of all arguments of all
// commands and continuous subcommands the given argument conflicts with.
//
// This behavior follows the propagation rules of global arguments. It is
// useful for finding conflicts for arguments declared as global.
//
// Panics if the given arg contains a conflict with an argument that is
// unknown to this `Command`.
func (c *Command) getGlobalArgConflictsWith(arg *Arg) []*Arg {
	result := []*Arg{}
	for _, id := range arg.blacklist {
		var found *Arg
		for _, candidate := range c.args.args {
			if candidate.GetId() == id {
				found = candidate
				break
			}
		}
		if found == nil {
		search:
			for _, sc := range c.getSubcommandsContaining(arg) {
				for _, candidate := range sc.args.args {
					if candidate.GetId() == id {
						found = candidate
						break search
					}
				}
			}
		}
		if found == nil {
			rs.PanicAt(1, "Command::get_arg_conflicts_with: The passed arg conflicts with an arg unknown to the cmd")
		}
		result = append(result, found)
	}
	return result
}

// getSubcommandsContaining gets a list of subcommands which contain the
// provided Argument.
//
// This command will only include subcommands in its list for which the
// subcommands parent also contains the Argument.
//
// This search follows the propagation rules of global arguments. It is
// useful to finding subcommands, that have inherited a global argument.
//
// NOTE: In this case only Sucommand_1 will be included
//
//	Subcommand_1 (contains Arg)
//	  Subcommand_1.1 (doesn't contain Arg)
//	    Subcommand_1.1.1 (contains Arg)
func (c *Command) getSubcommandsContaining(arg *Arg) []*Command {
	var vec []*Command
	for _, sc := range c.subcommands {
		contains := false
		for _, ar := range sc.args.args {
			if ar.GetId() == arg.GetId() {
				contains = true
				break
			}
		}
		if contains {
			vec = append(vec, sc)
			vec = append(vec, sc.getSubcommandsContaining(arg)...)
		}
	}
	return vec
}

// IsNoBinaryNameSet is Command::is_no_binary_name_set: report whether
// Command::no_binary_name is set.
func (c *Command) IsNoBinaryNameSet() bool {
	return c.isSet(appSettings_NoBinaryName)
}

// isIgnoreErrorsSet reports whether Command::ignore_errors is set.
func (c *Command) isIgnoreErrorsSet() bool {
	return c.isSet(appSettings_IgnoreErrors)
}

// IsDontDelimitTrailingValuesSet is
// Command::is_dont_delimit_trailing_values_set: report whether
// Command::dont_delimit_trailing_values is set.
func (c *Command) IsDontDelimitTrailingValuesSet() bool {
	return c.isSet(appSettings_DontDelimitTrailingValues)
}

// IsDisableVersionFlagSet is Command::is_disable_version_flag_set: report
// whether Command::disable_version_flag is set.
func (c *Command) IsDisableVersionFlagSet() bool {
	return c.isSet(appSettings_DisableVersionFlag) ||
		(c.version.IsNone() && c.longVersion.IsNone())
}

// IsPropagateVersionSet is Command::is_propagate_version_set: report
// whether Command::propagate_version is set.
func (c *Command) IsPropagateVersionSet() bool {
	return c.isSet(appSettings_PropagateVersion)
}

// IsNextLineHelpSet is Command::is_next_line_help_set: report whether
// Command::next_line_help is set.
func (c *Command) IsNextLineHelpSet() bool {
	return c.isSet(appSettings_NextLineHelp)
}

// IsDisableHelpFlagSet is Command::is_disable_help_flag_set: report whether
// Command::disable_help_flag is set.
func (c *Command) IsDisableHelpFlagSet() bool {
	return c.isSet(appSettings_DisableHelpFlag)
}

// IsDisableHelpSubcommandSet is Command::is_disable_help_subcommand_set:
// report whether Command::disable_help_subcommand is set.
func (c *Command) IsDisableHelpSubcommandSet() bool {
	return c.isSet(appSettings_DisableHelpSubcommand)
}

// IsDisableColoredHelpSet is Command::is_disable_colored_help_set: report
// whether Command::disable_colored_help is set.
func (c *Command) IsDisableColoredHelpSet() bool {
	return c.isSet(appSettings_DisableColoredHelp)
}

// isHelpExpectedSet reports whether Command::help_expected is set.
func (c *Command) isHelpExpectedSet() bool {
	return c.isSet(appSettings_HelpExpected)
}

// isInferLongArgsSet reports whether Command::infer_long_args is set.
func (c *Command) isInferLongArgsSet() bool {
	return c.isSet(appSettings_InferLongArgs)
}

// isInferSubcommandsSet reports whether Command::infer_subcommands is set.
func (c *Command) isInferSubcommandsSet() bool {
	return c.isSet(appSettings_InferSubcommands)
}

// IsArgRequiredElseHelpSet is Command::is_arg_required_else_help_set:
// report whether Command::arg_required_else_help is set.
func (c *Command) IsArgRequiredElseHelpSet() bool {
	return c.isSet(appSettings_ArgRequiredElseHelp)
}

func (c *Command) isAllowHyphenValuesSet() bool {
	return c.isSet(appSettings_AllowHyphenValues)
}

func (c *Command) isAllowNegativeNumbersSet() bool {
	return c.isSet(appSettings_AllowNegativeNumbers)
}

func (c *Command) isTrailingVarArgSet() bool {
	return c.isSet(appSettings_TrailingVarArg)
}

// IsAllowMissingPositionalSet is Command::is_allow_missing_positional_set:
// report whether Command::allow_missing_positional is set.
func (c *Command) IsAllowMissingPositionalSet() bool {
	return c.isSet(appSettings_AllowMissingPositional)
}

// IsHideSet is Command::is_hide_set: report whether Command::hide is set.
func (c *Command) IsHideSet() bool {
	return c.isSet(appSettings_Hidden)
}

// IsSubcommandRequiredSet is Command::is_subcommand_required_set: report
// whether Command::subcommand_required is set.
func (c *Command) IsSubcommandRequiredSet() bool {
	return c.isSet(appSettings_SubcommandRequired)
}

// IsAllowExternalSubcommandsSet is
// Command::is_allow_external_subcommands_set: report whether
// Command::allow_external_subcommands is set.
func (c *Command) IsAllowExternalSubcommandsSet() bool {
	return c.isSet(appSettings_AllowExternalSubcommands)
}

// GetExternalSubcommandValueParser is
// Command::get_external_subcommand_value_parser: configured parser for
// values passed to an external subcommand.
func (c *Command) GetExternalSubcommandValueParser() rs.Option[BuilderValueParser] {
	if !c.IsAllowExternalSubcommandsSet() {
		return rs.None[BuilderValueParser]()
	}
	return rs.Some(c.externalValueParser.UnwrapOrElse(BuilderValueParserOsString))
}

// IsArgsConflictsWithSubcommandsSet is
// Command::is_args_conflicts_with_subcommands_set: report whether
// Command::args_conflicts_with_subcommands is set.
func (c *Command) IsArgsConflictsWithSubcommandsSet() bool {
	return c.isSet(appSettings_ArgsNegateSubcommands)
}

// IsArgsOverrideSelf is Command::is_args_override_self.
func (c *Command) IsArgsOverrideSelf() bool {
	return c.isSet(appSettings_AllArgsOverrideSelf)
}

// IsSubcommandPrecedenceOverArgSet is
// Command::is_subcommand_precedence_over_arg_set: report whether
// Command::subcommand_precedence_over_arg is set.
func (c *Command) IsSubcommandPrecedenceOverArgSet() bool {
	return c.isSet(appSettings_SubcommandPrecedenceOverArg)
}

// IsSubcommandNegatesReqsSet is Command::is_subcommand_negates_reqs_set:
// report whether Command::subcommand_negates_reqs is set.
func (c *Command) IsSubcommandNegatesReqsSet() bool {
	return c.isSet(appSettings_SubcommandsNegateReqs)
}

// IsMulticallSet is Command::is_multicall_set: report whether
// Command::multicall is set.
func (c *Command) IsMulticallSet() bool {
	return c.isSet(appSettings_Multicall)
}

// # Internally used only

func (c *Command) getOverrideUsage() rs.Option[BuilderStyledStr] {
	return c.usageStr
}

func (c *Command) getOverrideHelp() rs.Option[BuilderStyledStr] {
	return c.helpStr
}

func (c *Command) getHelpTemplate() rs.Option[BuilderStyledStr] {
	return c.template
}

func (c *Command) getTermWidth() rs.Option[int] {
	return c.appExt.termWidth
}

func (c *Command) getMaxTermWidth() rs.Option[int] {
	return c.appExt.maxTermWidth
}

func (c *Command) getKeymap() *mkeymapMKeyMap {
	return &c.args
}

func (c *Command) getUsedGlobalArgs(matches *ArgMatches, globalArgVec *[]Id) {
	for _, a := range c.args.args {
		if a.IsGlobalSet() {
			*globalArgVec = append(*globalArgVec, a.id)
		}
	}
	if sub, ok := matches.Subcommand().Get(); ok {
		if usedSub := c.findSubcommand(sub.F0); usedSub != nil {
			usedSub.getUsedGlobalArgs(sub.F1, globalArgVec)
		}
	}
}

func (c *Command) doParse(rawArgs *lexRawArgs, argsCursor lexArgCursor) (*ArgMatches, *Error) {
	// If there are global arguments, or settings we need to propagate them down to subcommands
	// before parsing in case we run into a subcommand
	c.buildSelf(false)

	matcher := parserArgMatcherNew(c)

	// do the real parsing
	parser := parserParserNew(c)
	if err := parser.getMatchesWith(matcher, rawArgs, argsCursor); err != nil {
		if c.isSet(appSettings_IgnoreErrors) && err.UseStderr() {
		} else {
			return nil, err
		}
	}

	var globalArgVec []Id
	c.getUsedGlobalArgs(&matcher.matches, &globalArgVec)

	matcher.propagateGlobals(globalArgVec)

	return matcher.intoInner(), nil
}

// Build is Command::build: prepare for introspecting on all included
// Commands.
//
// Call this on the top-level Command when done building and before reading
// state for cases like completions, custom help output, etc.
func (c *Command) Build() {
	c.buildRecursive(true)
	c.buildBinNamesInternal()
}

func (c *Command) buildRecursive(expandHelpTree bool) {
	c.buildSelf(expandHelpTree)
	for _, subcmd := range c.subcommands {
		subcmd.buildRecursive(expandHelpTree)
	}
}

func (c *Command) buildSelf(expandHelpTree bool) {
	if !c.settings.isSet(appSettings_Built) {
		if deferred, ok := c.deferred.Take().Get(); ok {
			// `*self = (deferred)(std::mem::take(self))`
			taken := *c
			*c = *CommandDefault()
			result := deferred(&taken)
			*c = *result
		}

		// Make sure all the globally set flags apply to us as well
		c.settings = c.settings | c.gSettings

		if c.IsMulticallSet() {
			c.settings.set(appSettings_SubcommandRequired)
			c.settings.set(appSettings_DisableHelpFlag)
			c.settings.set(appSettings_DisableVersionFlag)
		}
		if c.isSet(appSettings_ArgsNegateSubcommands) {
			c.settings.set(appSettings_SubcommandsNegateReqs)
		}
		if c.externalValueParser.IsSome() {
			c.settings.set(appSettings_AllowExternalSubcommands)
		}
		if !c.HasSubcommands() {
			c.settings.set(appSettings_DisableHelpSubcommand)
		}

		c.propagate()
		c.checkHelpAndVersion(expandHelpTree)
		c.propagateGlobalArgs()

		posCounter := 1
		hidePv := c.isSet(appSettings_HidePossibleValues)
		for _, a := range c.args.args {
			// Fill in the groups
			for _, g := range a.groups {
				var found *ArgGroup
				for _, grp := range c.groups {
					if grp.id == g {
						found = grp
						break
					}
				}
				if found != nil {
					found.args = append(found.args, a.GetId())
				} else {
					ag := ArgGroupNew(string(g))
					ag.args = append(ag.args, a.GetId())
					c.groups = append(c.groups, ag)
				}
			}

			// Figure out implied settings
			a.build()
			if hidePv && a.isTakesValueSet() {
				a.settings.set(argSettings_HidePossibleValues)
			}
			if a.IsPositional() && a.index.IsNone() {
				a.index = rs.Some(posCounter)
				posCounter++
			}
		}

		c.args.build()

		{
			highestIdx := 0
			for i := range c.args.keys {
				if key := c.args.keys[i].key; key.kind == mkeymapKeyType_Position {
					highestIdx = max(highestIdx, key.position)
				}
			}
			isTrailingVarArgSet := c.isTrailingVarArgSet()
			isAllowHyphenValuesSet := c.isAllowHyphenValuesSet()
			isAllowNegativeNumbersSet := c.isAllowNegativeNumbersSet()
			for _, arg := range c.args.args {
				if isAllowHyphenValuesSet && arg.isTakesValueSet() {
					arg.settings.set(argSettings_AllowHyphenValues)
				}
				if isAllowNegativeNumbersSet && arg.isTakesValueSet() {
					arg.settings.set(argSettings_AllowNegativeNumbers)
				}
				if isTrailingVarArgSet && arg.GetIndex() == rs.Some(highestIdx) {
					arg.settings.set(argSettings_TrailingVarArg)
				}
			}
		}

		if debugAssertions {
			debugAssertsAssertApp(c)
		}
		c.settings.set(appSettings_Built)
	}
}

func (c *Command) buildSubcommand(name string) *Command {
	midString := " "
	if !c.IsSubcommandNegatesReqsSet() && !c.IsArgsConflictsWithSubcommandsSet() {
		reqs := outputUsageNew(c).getRequiredUsageFrom(nil, nil, true) // maybe Some(m)

		for _, s := range reqs {
			midString += s.String()
			midString += " "
		}
	}
	isMulticallSet := c.IsMulticallSet()

	var sc *Command
	for _, s := range c.subcommands {
		if s.name == name {
			sc = s
			break
		}
	}
	if sc == nil {
		return nil
	}

	// Display subcommand name, short and long in usage
	scNames := sc.name
	flagSubcmd := false
	if l, ok := sc.GetLongFlag().Get(); ok {
		scNames += "|--" + l
		flagSubcmd = true
	}
	if s, ok := sc.GetShortFlag().Get(); ok {
		scNames += "|-" + string(s)
		flagSubcmd = true
	}

	if flagSubcmd {
		scNames = "{" + scNames + "}"
	}

	usageName := scNames
	if binName, ok := c.binName.Get(); ok {
		usageName = binName + midString + scNames
	}
	sc.usageName = rs.Some(usageName)

	// bin_name should be parent's bin_name + [<reqs>] + the sc's name separated by
	// a space
	binName := c.binName.UnwrapOr("")
	if c.binName.IsSome() {
		binName += " "
	}
	binName += sc.name
	sc.binName = rs.Some(binName)

	if sc.displayName.IsNone() {
		var selfDisplayName string
		if isMulticallSet {
			selfDisplayName = c.displayName.UnwrapOr("")
		} else {
			selfDisplayName = c.displayName.UnwrapOr(c.name)
		}
		displayName := selfDisplayName
		if selfDisplayName != "" {
			displayName += "-"
		}
		displayName += sc.name
		sc.displayName = rs.Some(displayName)
	}

	// Ensure all args are built and ready to parse
	sc.buildSelf(false)

	return sc
}

func (c *Command) buildBinNamesInternal() {
	if !c.isSet(appSettings_BinNameBuilt) {
		midString := " "
		if !c.IsSubcommandNegatesReqsSet() && !c.IsArgsConflictsWithSubcommandsSet() {
			reqs := outputUsageNew(c).getRequiredUsageFrom(nil, nil, true) // maybe Some(m)

			for _, s := range reqs {
				midString += s.String()
				midString += " "
			}
		}
		isMulticallSet := c.IsMulticallSet()

		var selfBinName string
		if isMulticallSet {
			selfBinName = c.binName.UnwrapOr("")
		} else {
			selfBinName = c.binName.UnwrapOr(c.name)
		}

		for _, sc := range c.subcommands {
			if sc.usageName.IsNone() {
				// Display subcommand name, short and long in usage
				scNames := sc.name
				flagSubcmd := false
				if l, ok := sc.GetLongFlag().Get(); ok {
					scNames += "|--" + l
					flagSubcmd = true
				}
				if s, ok := sc.GetShortFlag().Get(); ok {
					scNames += "|-" + string(s)
					flagSubcmd = true
				}

				if flagSubcmd {
					scNames = "{" + scNames + "}"
				}

				usageName := selfBinName + midString + scNames
				sc.usageName = rs.Some(usageName)
			}

			if sc.binName.IsNone() {
				binName := selfBinName
				if selfBinName != "" {
					binName += " "
				}
				binName += sc.name
				sc.binName = rs.Some(binName)
			}

			if sc.displayName.IsNone() {
				var selfDisplayName string
				if isMulticallSet {
					selfDisplayName = c.displayName.UnwrapOr("")
				} else {
					selfDisplayName = c.displayName.UnwrapOr(c.name)
				}
				displayName := selfDisplayName
				if selfDisplayName != "" {
					displayName += "-"
				}
				displayName += sc.name
				sc.displayName = rs.Some(displayName)
			}

			sc.buildBinNamesInternal()
		}
		c.set(appSettings_BinNameBuilt)
	}
}

func (c *Command) panicOnMissingHelp(helpRequiredGlobally bool) {
	if c.isSet(appSettings_HelpExpected) || helpRequiredGlobally {
		var argsMissingHelp []string
		for _, arg := range c.args.args {
			if arg.GetHelp().IsNone() && arg.GetLongHelp().IsNone() {
				argsMissingHelp = append(argsMissingHelp, string(arg.GetId()))
			}
		}

		if debugAssertions && len(argsMissingHelp) != 0 {
			rs.PanicAt(1, "Command::help_expected is enabled for the Command "+c.name+
				", but at least one of its arguments does not have either `help` or `long_help` set. List of such arguments: "+
				strings.Join(argsMissingHelp, ", "))
		}
	}

	for _, subApp := range c.subcommands {
		subApp.panicOnMissingHelp(helpRequiredGlobally)
	}
}

// twoArgsOf returns the first two arguments that match the condition.
//
// If fewer than two arguments that match the condition, `None` is returned.
func (c *Command) twoArgsOf(condition func(*Arg) bool) (first *Arg, second *Arg, ok bool) {
	for _, a := range c.args.args {
		if !condition(a) {
			continue
		}
		if first == nil {
			first = a
			continue
		}
		return first, a, true
	}
	return nil, nil, false
}

// propagateGlobalArgs propagates global args.
func (c *Command) propagateGlobalArgs() {
	autogeneratedHelpSubcommand := !c.IsDisableHelpSubcommandSet()

	for _, sc := range c.subcommands {
		if sc.GetName() == "help" && autogeneratedHelpSubcommand {
			// Avoid propagating args to the autogenerated help subtrees used in completion.
			// This prevents args from showing up during help completions like
			// `myapp help subcmd <TAB>`, which should only suggest subcommands and not args,
			// while still allowing args to show up properly on the generated help message.
			continue
		}

		for _, a := range c.args.args {
			if !a.IsGlobalSet() {
				continue
			}
			if sc.find(a.id) != nil {
				continue
			}

			sc.args.push(a.Clone())
		}
	}
}

// propagate propagates settings.
func (c *Command) propagate() {
	subcommands := c.subcommands
	c.subcommands = nil
	for _, sc := range subcommands {
		c.propagateSubcommand(sc)
	}
	c.subcommands = subcommands
}

func (c *Command) propagateSubcommand(sc *Command) {
	// We have to create a new scope in order to tell rustc the borrow of `sc` is
	// done and to recursively call this method
	{
		if c.settings.isSet(appSettings_PropagateVersion) {
			if version, ok := c.version.Get(); ok {
				if sc.version.IsNone() {
					sc.version = rs.Some(version)
				}
			}
			if longVersion, ok := c.longVersion.Get(); ok {
				if sc.longVersion.IsNone() {
					sc.longVersion = rs.Some(longVersion)
				}
			}
		}

		sc.settings = sc.settings | c.gSettings
		sc.gSettings = sc.gSettings | c.gSettings
		sc.appExt.update(&c.appExt)
	}
}

func (c *Command) checkHelpAndVersion(expandHelpTree bool) {
	c.longHelpExists = c.longHelpExists_()

	if !c.IsDisableHelpFlagSet() {
		arg := ArgNew(idHelp).
			Short('h').
			Long("help").
			Action(ArgAction_Help)
		if c.longHelpExists {
			arg = arg.
				Help("Print help (see more with '--help')").
				LongHelp("Print help (see a summary with '-h')")
		} else {
			arg = arg.Help("Print help")
		}
		// Avoiding `arg_internal` to not be sensitive to `next_help_heading` /
		// `next_display_order`
		c.args.push(arg)
	}
	if !c.IsDisableVersionFlagSet() {
		arg := ArgNew(idVersion).
			Short('V').
			Long("version").
			Action(ArgAction_Version).
			Help("Print version")
		// Avoiding `arg_internal` to not be sensitive to `next_help_heading` /
		// `next_display_order`
		c.args.push(arg)
	}

	if !c.isSet(appSettings_DisableHelpSubcommand) {
		helpAbout := "Print this message or the help of the given subcommand(s)"

		var helpSubcmd *Command
		if expandHelpTree {
			// Slow code path to recursively clone all other subcommand subtrees under help
			helpSubcmd = CommandNew("help").
				About(helpAbout).
				globalSetting(appSettings_DisableHelpSubcommand)
			for _, sc := range c.subcommands {
				helpSubcmd = helpSubcmd.Subcommand(sc.copySubtreeForHelp())
			}

			helpHelpSubcmd := CommandNew("help").About(helpAbout)
			helpHelpSubcmd.version = rs.None[string]()
			helpHelpSubcmd.longVersion = rs.None[string]()
			helpHelpSubcmd = helpHelpSubcmd.
				setting(appSettings_DisableHelpFlag).
				setting(appSettings_DisableVersionFlag)

			helpSubcmd = helpSubcmd.Subcommand(helpHelpSubcmd)
		} else {
			helpSubcmd = CommandNew("help").About(helpAbout).Arg(
				ArgNew("subcommand").
					Action(ArgAction_Append).
					NumArgs(BuilderValueRangeFromRangeFull()).
					ValueName("COMMAND").
					Help("Print help for the subcommand(s)"),
			)
		}
		c.propagateSubcommand(helpSubcmd)

		// The parser acts like this is set, so let's set it so we don't falsely
		// advertise it to the user
		helpSubcmd.version = rs.None[string]()
		helpSubcmd.longVersion = rs.None[string]()
		helpSubcmd = helpSubcmd.
			setting(appSettings_DisableHelpFlag).
			setting(appSettings_DisableVersionFlag).
			unsetGlobalSetting(appSettings_PropagateVersion)

		c.subcommands = append(c.subcommands, helpSubcmd)
	}
}

func (c *Command) copySubtreeForHelp() *Command {
	cmd := CommandNew(c.name).
		Hide(c.IsHideSet()).
		globalSetting(appSettings_DisableHelpFlag).
		globalSetting(appSettings_DisableVersionFlag)
	for _, sc := range c.subcommands {
		cmd = cmd.Subcommand(sc.copySubtreeForHelp())
	}
	if about, ok := c.GetAbout().Get(); ok {
		cmd.about = rs.Some(about)
	}
	return cmd
}

func (c *Command) renderVersion(useLong bool) string {
	var ver string
	if useLong {
		ver = c.longVersion.Or(c.version).UnwrapOr("")
	} else {
		ver = c.version.Or(c.longVersion).UnwrapOr("")
	}
	displayName := c.GetDisplayName().UnwrapOrElse(c.GetName)
	return displayName + " " + ver + "\n"
}

func (c *Command) formatGroup(g Id) BuilderStyledStr {
	var names []string
	for _, x := range c.unrollArgsInGroup(g) {
		arg := c.find(x)
		if arg == nil {
			continue
		}
		if arg.IsPositional() {
			// Print val_name for positional arguments. e.g. <file_name>
			names = append(names, arg.nameNoBrackets())
		} else {
			// Print usage string for flags arguments, e.g. <--help>
			names = append(names, arg.String())
		}
	}
	gString := strings.Join(names, "|")
	placeholder := c.GetStyles().GetPlaceholder()
	styled := BuilderStyledStrNew()
	styled.pushString(placeholder.Render() + "<" + gString + ">" + placeholder.RenderReset())
	return styled
}

// # Internal Query Methods

// getNonPositionals iterates through the *flags* & *options* arguments.
func (c *Command) getNonPositionals() []*Arg {
	var args []*Arg
	for _, a := range c.args.args {
		if !a.IsPositional() {
			args = append(args, a)
		}
	}
	return args
}

func (c *Command) find(argId Id) *Arg {
	for _, a := range c.args.args {
		if a.GetId() == argId {
			return a
		}
	}
	return nil
}

func (c *Command) containsShort(s rune) bool {
	if debugAssertions && !c.isSet(appSettings_Built) {
		rs.PanicAt(1, "If Command::_build hasn't been called, manually search through Arg shorts")
	}

	return c.args.containsShort(s)
}

func (c *Command) set(s appSettings) {
	c.settings.set(s)
}

func (c *Command) hasPositionals() bool {
	for _, a := range c.args.args {
		if a.IsPositional() {
			return true
		}
	}
	return false
}

func (c *Command) hasVisibleSubcommands() bool {
	for _, sc := range c.subcommands {
		if sc.name != "help" && !sc.isSet(appSettings_Hidden) {
			return true
		}
	}
	return false
}

// aliasesTo checks if this subcommand can be referred to as `name`. In
// other words, check if `name` is the name of this subcommand or is one of
// its aliases.
func (c *Command) aliasesTo(name string) bool {
	if c.GetName() == name {
		return true
	}
	for _, alias := range c.aliases {
		if alias.name == name {
			return true
		}
	}
	return false
}

// shortFlagAliasesTo checks if this subcommand can be referred to as
// `name`. In other words, check if `name` is the name of this short flag
// subcommand or is one of its short flag aliases.
func (c *Command) shortFlagAliasesTo(flag rune) bool {
	if rs.Some(flag) == c.shortFlag {
		return true
	}
	for _, alias := range c.shortFlagAliases {
		if alias.name == flag {
			return true
		}
	}
	return false
}

// longFlagAliasesTo checks if this subcommand can be referred to as `name`.
// In other words, check if `name` is the name of this long flag subcommand
// or is one of its long flag aliases.
func (c *Command) longFlagAliasesTo(flag string) bool {
	if longFlag, ok := c.longFlag.Get(); ok {
		if longFlag == flag {
			return true
		}
	}
	for _, alias := range c.longFlagAliases {
		if alias.name == flag {
			return true
		}
	}
	return false
}

func (c *Command) idExists(id Id) bool {
	for _, x := range c.args.args {
		if x.GetId() == id {
			return true
		}
	}
	for _, x := range c.groups {
		if x.id == id {
			return true
		}
	}
	return false
}

// groupsForArg iterates through the groups this arg is member of.
func (c *Command) groupsForArg(arg Id) []Id {
	var ids []Id
	for _, grp := range c.groups {
		if idsContain(grp.args, arg) {
			ids = append(ids, grp.id)
		}
	}
	return ids
}

func (c *Command) findGroup(groupId Id) *ArgGroup {
	for _, g := range c.groups {
		if g.id == groupId {
			return g
		}
	}
	return nil
}

// allSubcommandNames iterates through all the names of all subcommands (not
// recursively), including aliases. Used for suggestions.
func (c *Command) allSubcommandNames() []string {
	var names []string
	for _, sc := range c.subcommands {
		names = append(names, sc.GetName())
		names = append(names, sc.GetAllAliases()...)
	}
	return names
}

func (c *Command) requiredGraph() *utilChildGraph {
	reqs := utilChildGraphWithCapacity(5)
	for _, a := range c.args.args {
		if a.IsRequiredSet() {
			reqs.insert(a.GetId())
		}
	}
	for _, group := range c.groups {
		if group.required {
			idx := reqs.insert(group.id)
			for _, a := range group.requires {
				reqs.insertChild(idx, a)
			}
		}
	}

	return reqs
}

func (c *Command) unrollArgsInGroup(group Id) []Id {
	gVec := []Id{group}
	args := []Id{}

	for len(gVec) > 0 {
		g := gVec[len(gVec)-1]
		gVec = gVec[:len(gVec)-1]
		grp := c.findGroup(g)
		if grp == nil {
			rs.PanicAt(1, internalErrorMsg)
		}
		for _, n := range grp.args {
			if !idsContain(args, n) {
				if c.find(n) != nil {
					args = append(args, n)
				} else {
					gVec = append(gVec, n)
				}
			}
		}
	}

	return args
}

func (c *Command) unrollArgRequires(isRelevant func(*argRequire) (Id, bool), arg Id) []Id {
	var processed []Id
	rVec := []Id{arg}
	args := []Id{}

	for len(rVec) > 0 {
		a := rVec[len(rVec)-1]
		rVec = rVec[:len(rVec)-1]
		if idsContain(processed, a) {
			continue
		}

		processed = append(processed, a)

		if found := c.find(a); found != nil {
			for i := range found.requires {
				r, ok := isRelevant(&found.requires[i])
				if !ok {
					continue
				}
				if req := c.find(r); req != nil {
					if len(req.requires) != 0 {
						rVec = append(rVec, req.GetId())
					}
				}
				args = append(args, r)
			}
		}
	}

	return args
}

// findShortSubcmd finds a flag subcommand name by short flag or an alias.
func (c *Command) findShortSubcmd(flag rune) rs.Option[string] {
	for _, sc := range c.subcommands {
		if sc.shortFlagAliasesTo(flag) {
			return rs.Some(sc.GetName())
		}
	}
	return rs.None[string]()
}

// findLongSubcmd finds a flag subcommand name by long flag or an alias.
func (c *Command) findLongSubcmd(long string) rs.Option[string] {
	for _, sc := range c.subcommands {
		if sc.longFlagAliasesTo(long) {
			return rs.Some(sc.GetName())
		}
	}
	return rs.None[string]()
}

func (c *Command) writeHelpErr(useLong bool) BuilderStyledStr {
	useLong = useLong && c.longHelpExistsGet()
	usage := outputUsageNew(c)

	styled := BuilderStyledStrNew()
	outputWriteHelp(&styled, c, usage, useLong)

	return styled
}

func (c *Command) writeVersionErr(useLong bool) BuilderStyledStr {
	msg := c.renderVersion(useLong)
	return BuilderStyledStr(msg)
}

func (c *Command) longHelpExistsGet() bool {
	return c.longHelpExists
}

func (c *Command) longHelpExists_() bool {
	// In this case, both must be checked. This allows the retention of
	// original formatting, but also ensures that the actual -h or --help
	// specified by the user is sent through. If hide_short_help is not included,
	// then items specified with hidden_short_help will also be hidden.
	shouldLong := func(v *Arg) bool {
		if v.IsHideSet() {
			return false
		}
		if v.GetLongHelp().IsSome() || v.IsHideLongHelpSet() || v.IsHideShortHelpSet() {
			return true
		}
		if !v.IsHidePossibleValuesSet() {
			possibleValues := v.GetPossibleValues()
			for i := range possibleValues {
				if possibleValues[i].shouldShowHelp() {
					return true
				}
			}
		}
		return false
	}

	// Subcommands aren't checked because we prefer short help for them, deferring to
	// `cmd subcmd --help` for more.
	if c.GetLongAbout().IsSome() || c.GetBeforeLongHelp().IsSome() || c.GetAfterLongHelp().IsSome() {
		return true
	}
	for _, a := range c.args.args {
		if shouldLong(a) {
			return true
		}
	}
	return false
}

// colorHelp: should we color the help?
func (c *Command) colorHelp() ColorChoice {
	if c.IsDisableColoredHelpSet() {
		return ColorChoice_Never
	}

	return c.GetColor()
}

// Clone is `impl Clone for Command`; `Command::from(&cmd)` is the same.
func (c *Command) Clone() *Command {
	cloned := *c
	cloned.aliases = append([]commandAlias(nil), c.aliases...)
	cloned.shortFlagAliases = append([]commandShortAlias(nil), c.shortFlagAliases...)
	cloned.longFlagAliases = append([]commandAlias(nil), c.longFlagAliases...)
	cloned.args = c.args.clone()
	cloned.subcommands = make([]*Command, len(c.subcommands))
	for i, sc := range c.subcommands {
		cloned.subcommands[i] = sc.Clone()
	}
	if len(c.subcommands) == 0 {
		cloned.subcommands = nil
	}
	cloned.groups = make([]*ArgGroup, len(c.groups))
	for i, g := range c.groups {
		cloned.groups[i] = g.Clone()
	}
	if len(c.groups) == 0 {
		cloned.groups = nil
	}
	return &cloned
}

// String is `impl Display for Command`: the name.
func (c *Command) String() string {
	return c.name
}

// FmtDisplay is `impl Display for Command`.
func (c *Command) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(c.name)
}
