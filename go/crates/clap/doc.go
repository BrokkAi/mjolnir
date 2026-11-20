// Package clap stands in for the Rust crate clap 4.6.1: clap_builder 4.6.0
// and what clap_derive 4.6.1 generates, with clap_lex 1.1.0, anstyle
// 1.0.14 (as clap::builder::styling), anstream 1.0.0, anstyle-parse 1.0.0,
// anstyle-query 1.1.5 and strsim 0.11.1 (Jaro similarity for suggestions).
//
// The crate is built with the features Mjolnir's Cargo.lock resolves: the
// defaults (std, color, help, usage, error-context, suggestions) plus derive
// and env. Not wrap_help, so help text is never wrapped and the terminal
// size is never asked for: help is laid out for 100 columns unless
// Command::term_width or max_term_width says otherwise (0 means no limit).
// Not unicode, so every character counts as one column.
//
// # Names
//
// Names follow .agents/docs/go-port/translation-rules.md over the paths
// Rust code writes: clap::Command is Command, clap::Arg is Arg,
// clap::ArgGroup is ArgGroup, clap::ArgMatches is ArgMatches, clap::Error is
// Error, clap::Id is Id, clap::ArgAction::Set is ArgAction_Set,
// clap::ColorChoice is ColorChoice, clap::ValueHint is ValueHint,
// clap::builder::ValueParser is BuilderValueParser, clap::builder::
// PossibleValue is BuilderPossibleValue, clap::builder::StyledStr is
// BuilderStyledStr, clap::builder::styling::Styles is BuilderStyles (and
// AnsiColor, Style, Effects, ... are BuilderStyling*), clap::builder::
// ArgPredicate is BuilderArgPredicate, clap::error::ErrorKind is
// ErrorErrorKind, clap::error::ContextKind and ContextValue are
// ErrorContextKind and ErrorContextValue, clap::parser::ValueSource is
// ParserValueSource. Associated functions are type plus name
// (Command::new is CommandNew, Arg::new is ArgNew, ArgGroup::new is
// ArgGroupNew, Error::raw is ErrorRaw). Builder methods keep their names and
// chain: CommandNew("mj").Arg(ArgNew("json").Long("json")). A builder
// method whose Rust parameter takes `None` to reset is the method of the
// same name with Reset appended (Arg::help(None) is HelpReset()). Rust's
// generic ArgMatches methods are package functions with a type argument:
// `matches.get_one::<String>("name")` is ArgMatchesGetOne[string](m, "name"),
// likewise ArgMatchesGetMany, ArgMatchesRemoveOne, ArgMatchesRemoveMany,
// ArgMatchesGetOccurrences, ArgMatchesTry*. `value_parser!(T)` is
// ValueParserString, ValueParserPathBuf, ValueParserBool, ValueParserU8 ...
// ValueParserI64, ValueParserUsize, ValueParserF64, ValueParserEnum[E] for a
// ValueEnum, ValueParserFromStr(T_from_str) for a FromStr type. A value
// parser that is a function is BuilderValueParserFromFn(f).
//
// Value types: String, OsString and PathBuf values are Go strings (so
// ArgMatchesGetOne[string] reads any of them; Rust's type-mismatch error
// between those three cannot happen in Go); usize and isize are int; a
// count (ArgAction::Count) is uint8; a group's values are Id.
//
// # What is covered
//
// All of clap_builder that Mjolnir's definitions reach, file for file:
// builder (Command, Arg, ArgGroup, ArgAction, ArgPredicate, ValueRange,
// ValueParser and every typed parser, PossibleValue, ValueHint, StyledStr,
// Styles, the settings, debug_asserts), parser (lexing through clap_lex,
// Parser, ArgMatcher, ArgMatches, MatchedArg, ValueSource, MatchesError,
// Validator, the suggestions), error (Error, ErrorKind, ContextKind,
// ContextValue, RichFormatter and KindFormatter), output (Usage, the help
// template and its default layout, textwrap, the colorizer), derive.rs (the
// Parser, ValueEnum traits; see below), mkeymap and util. Styling follows
// anstyle's escape sequences exactly, and printing follows anstream: the
// output keeps its escape sequences when the color choice is Always, or
// Auto on a terminal with NO_COLOR unset or empty, CLICOLOR_FORCE unset or
// empty, CLICOLOR not "0", and TERM set and not "dumb" (or CLICOLOR set, or
// CI set); otherwise they are stripped. On Windows the console is switched
// to virtual terminal processing, as anstyle-query does.
//
// Exit codes and streams are clap's: Error.Exit writes help and version to
// standard output and exits 0, and every other error to standard error and
// exits 2, through rsio's standard streams (so the output is ordered with
// the program's own println! output, as in Rust).
//
// # What is not covered
//
//   - The wrap_help and unicode features (see above), so textwrap's wrapping
//     runs only behind featureWrapHelp, which is off.
//   - Error.Apply (an Error formatted by another ErrorFormatter) and the
//     Debug form of Error (its derived Debug of the inner state): rs.Todo.
//   - anstream's wincon path, which turns escape codes into console calls on
//     a Windows console that cannot enable virtual terminal processing
//     (before Windows 10): rs.Todo.
//   - A usize value above 2^63-1 (Go's int) is refused as too large, where
//     Rust on a 64-bit target accepts it.
//   - Command-line arguments that are not UTF-8 are Go strings holding the
//     same bytes; they are handled as clap handles an OsStr that is not
//     UTF-8, but cannot reach the oracle's corpus.
//   - clap_complete, clap_mangen, unstable features, the `arg!` and
//     `command!` macros: Mjolnir uses none of them.
//   - debug_asserts sorts its flag lists with a stable sort; Rust's
//     sort_unstable differs only in the order it names two duplicate flags
//     in the panic message, on lists longer than twenty.
//
// Debug assertions (debug_asserts, the matcher's id checks) run when the
// build tag mj_debug is set, as Rust runs them in its development profile.
//
// # The attributes Mjolnir uses
//
// Mjolnir defines its command lines with derives in mj-cli/src (main.rs,
// api_commands.rs, import.rs, acp.rs, go.rs, simple.rs, image_commands.rs,
// ec2.rs, ui_command.rs), mj-worker-bin/src/main.rs and
// mj-worker/src/microvm/cli.rs: 96 items deriving Parser, Subcommand, Args
// or ValueEnum. The attributes, with how often they appear:
//
//	#[command(...)]: subcommand (18), hide = true (17), flatten (9),
//	  group(ArgGroup::new(..).args([..]).required(..).multiple(..)) (4),
//	  name = ".." (3), version (3), about = ".." (3)
//	#[arg(...)]: long (318 bare, 19 with a value), short (19 bare, 2 with
//	  a value), conflicts_with (19), conflicts_with_all (1), value_name (17),
//	  requires (12), hide = true (12), default_value_t (7), required = true
//	  (7), required_unless_present (6), required_if_eq (1), value_enum (6),
//	  global = true (5), value_parser (8: value_parser!(u64).range(1..), a
//	  list of strings, a function), value_delimiter = ',' (4), action =
//	  ArgAction::Set (2) and Append (1), allow_hyphen_values (3),
//	  trailing_var_arg (3), env = ".." (2), id = ".." (1), visible_alias (1)
//	doc comments on items, fields and variants (help, about, long_help)
//
// and the field types Option<String> (114), bool (106), String (57),
// Vec<String> (26), PathBuf (25), Option<PathBuf> (16), Option<u64> (11),
// usize, Option<usize>, u32, Option<u32>, Option<i32>, Option<f64>,
// Vec<PathBuf>, value enums, FromStr types (HarnessKind, SubagentMcpRole,
// TranscriptRole), flattened Args structs, Option<Args>, subcommand enums
// and Option<subcommand>. Code also calls Cli::parse, Cli::try_parse_from,
// <Cli as CommandFactory>::command, Command::find_subcommand,
// get_arguments, get_subcommands, get_name, get_id, get_long, get_help,
// get_long_help, is_hide_set and render_help.
//
// # The Go spelling of a derive
//
// A derive is transcribed by hand, mechanically, from what clap_derive
// generates. The Rust type keeps its fields (translation rules 2 and 3), and
// each trait the derive implements becomes package-level functions named
// after the type (translation rule 2: a trait function without self is
// T_<function>):
//
//	#[derive(Parser)] or #[derive(Args)] on struct T:
//	    T_augment_args(cmd *clap.Command) *clap.Command    Args::augment_args
//	    T_group_id() rs.Option[clap.Id]                     Args::group_id
//	    T_from_arg_matches_mut(m *clap.ArgMatches) (T, error)
//	#[derive(Parser)] also:
//	    T_command() *clap.Command                           CommandFactory::command
//	    var T_Parser = clap.Parser[T]{Command: T_command, FromArgMatchesMut: T_from_arg_matches_mut}
//	    and Cli::parse() is T_Parser.Parse(), Cli::try_parse_from(argv)
//	    is T_Parser.TryParseFrom(argv)
//	#[derive(Subcommand)] on enum E:
//	    E_augment_subcommands(cmd *clap.Command) *clap.Command
//	    E_has_subcommand(name string) bool
//	    E_from_arg_matches_mut(m *clap.ArgMatches) (E, error)
//	#[derive(ValueEnum)] on a data-free enum E (a uint8 type):
//	    func (E) ValueVariants() []E and
//	    func (e E) ToPossibleValue() rs.Option[clap.BuilderPossibleValue]
//
// The _for_update variants and update_from_arg_matches are written only
// where the program calls update_from, which Mjolnir does not.
//
// T_augment_args does what the derive's augment_args does, in its order:
//
//  1. cmd.NextDisplayOrder / NextHelpHeading for those attributes on T.
//  2. The group the derive adds unless T has #[group(skip)]: id = the Rust
//     type name, multiple(true), the group's own #[group(..)] calls, and
//     .Args(ids of the plain argument fields) — no ids at all when any field
//     is #[command(flatten)].
//  3. For each field in order:
//     - #[command(flatten)] field of type U: cmd =
//     U_augment_args(cmd) (an Option<U> first panics, as Rust does, if U has
//     no group id).
//     - #[command(subcommand)] field of enum type U: cmd =
//     U_augment_subcommands(cmd), then .SubcommandRequired(true).
//     ArgRequiredElseHelp(true) unless the field is an Option.
//     - any other field: cmd.Arg(a) where a = clap.ArgNew(id), id being
//     the field's name as written (`id = ".."` replaces it), then the
//     implicit calls, then the explicit ones. Implicit, by field type:
//     T or bool (the derive's Ty::Other):
//     .ValueName(NAME).Required(R).ValueParser(VP).Action(A),
//     R = no default_value* attribute && A takes values;
//     Option<T>: .ValueName(NAME).ValueParser(VP).Action(A);
//     Option<Option<T>>: .ValueName(NAME).NumArgs(0..=1).ValueParser(VP).Action(A);
//     Vec<T> and Option<Vec<T>>: .ValueName(NAME), .NumArgs(1..) when the
//     field is positional (has neither short nor long), .ValueParser(VP).Action(A).
//     NAME is the field name in SCREAMING_SNAKE_CASE; VP is the
//     value_parser attribute or value_parser!(inner type); A is the action
//     attribute or, by type, Append for Vec, Set for Option, SetTrue for
//     bool, Set otherwise. The explicit calls follow: first the doc comment
//     (.Help(short) and, when it has several paragraphs, .LongHelp(long)),
//     then each attribute in source order as the method of the same name:
//     bare `long` is .Long(field name in kebab-case), bare `short` is
//     .Short(first character of the kebab-case name), bare `env` is
//     .Env(SCREAMING_SNAKE name), `default_value_t = v` is
//     .DefaultValue(v.to_string()) (or the possible value's name with
//     value_enum), `value_name = ".."` is .ValueName(".."), and so on.
//  4. The struct's own calls: the doc comment (.About(short) and
//     .LongAbout(long)), then its attributes in order (`version` without a
//     value is .Version(CARGO_PKG_VERSION), `about = ".."` is .About(..),
//     `hide = true` is .Hide(true)).
//
// A doc comment becomes help text as clap_derive's format_doc_comment
// makes it: the lines without their leading `/// `, blank lines kept as
// paragraph breaks; the short form is the first paragraph with its lines
// joined by single spaces and one trailing period removed (not one of
// "..."); the long form, set only when there are several paragraphs, is
// every paragraph joined that way and separated by a blank line, periods
// kept.
//
// E_augment_subcommands adds, for each variant in order, the subcommand
// clap.CommandNew(variant name in kebab-case, or `name = ".."`) built as
// follows: a unit variant gets the variant's calls (doc comment as about,
// then attributes); a newtype variant V(U) gets U_augment_args (or, with
// #[command(subcommand)], .SubcommandRequired(true).ArgRequiredElseHelp(true)
// and U_augment_subcommands) and then the variant's calls; a struct variant
// is treated as an Args struct named after the variant (its group id is the
// variant's name). E_has_subcommand compares with those names.
//
// T_from_arg_matches_mut builds T field by field, in order: a plain field
// is ArgMatchesRemoveOne[T](m, id) — for T or bool an absent value is
// ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required
// argument was not provided: " + id) —, an Option<T> is the RemoveOne
// result, a Vec<T> is ArgMatchesRemoveMany (empty when absent),
// Option<Option<T>> and Option<Vec<T>> are Some only when m.ContainsId(id);
// a flattened field is U_from_arg_matches_mut(m) (an Option<U> is Some only
// when m.ContainsId(U's group id)); a subcommand field is
// U_from_arg_matches_mut(m) (an Option<U> is Some only when
// m.SubcommandName() names one of U's subcommands).
// E_from_arg_matches_mut takes m.RemoveSubcommand(); with none it is
// ErrorRaw(ErrorErrorKind_MissingSubcommand, "a subcommand is required but
// one was not provided"); it matches the name against each variant whose
// matches do not contain the id "" and builds the variant from the
// subcommand's matches; no match is ErrorRaw(ErrorErrorKind_InvalidSubcommand,
// "the subcommand '<name>' wasn't recognized").
//
// The example below is the start of mj-worker's command line
// (mj-worker-bin/src/main.rs) and its transcription; the whole of it is in
// derive_mjworker_test.go, which checks its parsing, errors and help
// against the real derive run by the oracle.
//
//	#[derive(Debug, Parser)]
//	#[command(name = "mj-worker", version, about = "Mjolnir target-side worker")]
//	struct Cli {
//	    /// Internal handoff: the parent explicitly supplied the clean login snapshot.
//	    #[arg(long, hide = true, global = true)]
//	    login_environment_ready: bool,
//	    /// Descriptor transferred by login-environment re-exec.
//	    #[arg(long, hide = true, global = true)]
//	    worker_root_lock_fd: Option<i32>,
//	    #[command(subcommand)]
//	    command: Command,
//	}
//
//	#[derive(Debug, Subcommand)]
//	enum Command {
//	    /// Internal target-side worker commands.
//	    #[command(hide = true)]
//	    Worker(WorkerArgs),
//	}
//
//	#[derive(Debug, Args)]
//	struct WorkerArgs {
//	    #[command(subcommand)]
//	    command: WorkerCommand,
//	}
//
//	#[derive(Debug, Subcommand)]
//	enum WorkerCommand {
//	    /// Print the pid of the worker holding a root's lock, or nothing.
//	    #[command(hide = true)]
//	    Holder {
//	        #[arg(long)]
//	        root: PathBuf,
//	    },
//	    ...
//	}
//
// is, in Go (the mj-worker-bin crate is package main, so its private items
// are main_...):
//
//	type main_Cli struct {
//		loginEnvironmentReady bool              `rs:"login_environment_ready"`
//		workerRootLockFd      rs.Option[int32]  `rs:"worker_root_lock_fd"`
//		command               main_Command      `rs:"command"`
//	}
//
//	// #[derive(Parser)] for Cli
//
//	func main_Cli_command() *clap.Command {
//		return main_Cli_augment_args(clap.CommandNew("mj-worker"))
//	}
//
//	var main_Cli_Parser = clap.Parser[main_Cli]{
//		Command:           main_Cli_command,
//		FromArgMatchesMut: main_Cli_from_arg_matches_mut,
//	}
//
//	func main_Cli_group_id() rs.Option[clap.Id] { return rs.Some(clap.Id("Cli")) }
//
//	func main_Cli_augment_args(cmd *clap.Command) *clap.Command {
//		cmd = cmd.Group(clap.ArgGroupNew("Cli").Multiple(true).
//			Args("login_environment_ready", "worker_root_lock_fd"))
//		cmd = cmd.Arg(clap.ArgNew("login_environment_ready").
//			ValueName("LOGIN_ENVIRONMENT_READY").Required(false).
//			ValueParser(clap.ValueParserBool()).Action(clap.ArgAction_SetTrue).
//			Help("Internal handoff: the parent explicitly supplied the clean login snapshot").
//			Long("login-environment-ready").Hide(true).Global(true))
//		cmd = cmd.Arg(clap.ArgNew("worker_root_lock_fd").
//			ValueName("WORKER_ROOT_LOCK_FD").
//			ValueParser(clap.ValueParserI32()).Action(clap.ArgAction_Set).
//			Help("Descriptor transferred by login-environment re-exec").
//			Long("worker-root-lock-fd").Hide(true).Global(true))
//		cmd = main_Command_augment_subcommands(cmd)
//		cmd = cmd.SubcommandRequired(true).ArgRequiredElseHelp(true)
//		return cmd.Version(mjWorkerVersion).About("Mjolnir target-side worker")
//	}
//
//	func main_Cli_from_arg_matches_mut(m *clap.ArgMatches) (main_Cli, error) {
//		loginEnvironmentReady, ok := clap.ArgMatchesRemoveOne[bool](m, "login_environment_ready").Get()
//		if !ok {
//			return main_Cli{}, clap.ErrorRaw(clap.ErrorErrorKind_MissingRequiredArgument,
//				"the following required argument was not provided: login_environment_ready")
//		}
//		workerRootLockFd := clap.ArgMatchesRemoveOne[int32](m, "worker_root_lock_fd")
//		command, err := main_Command_from_arg_matches_mut(m)
//		if err != nil {
//			return main_Cli{}, err
//		}
//		return main_Cli{
//			loginEnvironmentReady: loginEnvironmentReady,
//			workerRootLockFd:      workerRootLockFd,
//			command:               command,
//		}, nil
//	}
//
//	// #[derive(Subcommand)] for Command
//
//	func main_Command_augment_subcommands(cmd *clap.Command) *clap.Command {
//		sub := clap.CommandNew("worker")
//		sub = main_WorkerArgs_augment_args(sub)
//		sub = sub.About("Internal target-side worker commands").Hide(true)
//		return cmd.Subcommand(sub)
//	}
//
//	func main_Command_has_subcommand(name string) bool { return name == "worker" }
//
//	func main_Command_from_arg_matches_mut(m *clap.ArgMatches) (main_Command, error) {
//		sc, ok := m.RemoveSubcommand().Get()
//		if !ok {
//			return nil, clap.ErrorRaw(clap.ErrorErrorKind_MissingSubcommand,
//				"a subcommand is required but one was not provided")
//		}
//		name, sub := sc.F0, sc.F1
//		if name == "worker" && !sub.ContainsId("") {
//			args, err := main_WorkerArgs_from_arg_matches_mut(sub)
//			if err != nil {
//				return nil, err
//			}
//			return &main_Command_Worker{F0: args}, nil
//		}
//		return nil, clap.ErrorRaw(clap.ErrorErrorKind_InvalidSubcommand,
//			"the subcommand '"+name+"' wasn't recognized")
//	}
//
//	// #[derive(Args)] for WorkerArgs: a struct whose only field is a
//	// subcommand still gets its group, with no ids.
//
//	func main_WorkerArgs_augment_args(cmd *clap.Command) *clap.Command {
//		cmd = cmd.Group(clap.ArgGroupNew("WorkerArgs").Multiple(true).Args())
//		cmd = main_WorkerCommand_augment_subcommands(cmd)
//		return cmd.SubcommandRequired(true).ArgRequiredElseHelp(true)
//	}
//
//	// #[derive(Subcommand)] for WorkerCommand: the struct variant Holder.
//
//	func main_WorkerCommand_augment_subcommands(cmd *clap.Command) *clap.Command {
//		...
//		sub := clap.CommandNew("holder")
//		sub = sub.Group(clap.ArgGroupNew("Holder").Multiple(true).Args("root"))
//		sub = sub.Arg(clap.ArgNew("root").ValueName("ROOT").Required(true).
//			ValueParser(clap.ValueParserPathBuf()).Action(clap.ArgAction_Set).
//			Long("root"))
//		sub = sub.About("Print the pid of the worker holding a root's lock, or nothing").Hide(true)
//		cmd = cmd.Subcommand(sub)
//		...
//	}
package clap
