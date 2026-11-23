// Part of the port of mj-worker-bin/src/main.rs: the clap-derived command
// line (`Cli`, `Command`, `WorkerArgs`, `AuthorizedKeysArgs` and their
// subcommands).
//
// Rust derives the parser with clap 4. Here the command tree is cobra's and
// each option is a pflag flag whose annotations carry what clap knows and
// pflag does not (value name, requirement, defaults, value lists, conflicts
// and requirements). Parsing walks that tree the way clap's parser does, so
// help, version and usage errors read as clap's and leave with clap's
// statuses: 0 for help and version on standard output, 2 for a usage error
// on standard error. The controller's control scripts probe a worker with
// `worker <subcommand> --help` and take status 2 for a worker too old to know
// the subcommand, so those statuses are part of the worker's contract
// (testdata/tests/golden/worker_control_contract.txt in go/cmd/mj-worker).
//
// clap's derive names each option after its field in kebab case
// (`expected_sha256` is `--expected-sha256`, `_root` is `--root`) and its
// value after the field in upper snake case; an option of a plain type is
// required, an Option or bool is not, and a Vec is repeatable. The hidden
// global options may follow the subcommand, which is where the login re-exec
// appends them. The workspace builds clap without `wrap_help` or `unicode`,
// so help text is never wrapped and widths count characters. Colours are not
// rendered: a worker's output is read by its controller, never a terminal.

package workerbin

import (
	"cmp"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// cliBinaryName is the program name (clap's `#[command(name)]`).
const cliBinaryName = "mj-worker"

// cliAbout is the program's about text.
const cliAbout = "Mjolnir target-side worker"

// clapErrorKind is how parsing ended without a command to run (the kinds of
// clap's ErrorKind this command line produces).
type clapErrorKind int

const (
	// clapErrorDisplayHelp: `--help`/`-h`, or the `help` subcommand.
	clapErrorDisplayHelp clapErrorKind = iota
	// clapErrorDisplayVersion: `--version`/`-V`.
	clapErrorDisplayVersion
	// clapErrorHelpOnMissingSubcommand: a command that needs a subcommand
	// was given nothing at all, so clap shows its help as the error.
	clapErrorHelpOnMissingSubcommand
	// clapErrorUsage: anything clap rejects.
	clapErrorUsage
)

// clapError is a parse that ended without a command: help or version to
// print, or a usage error.
type clapError struct {
	kind clapErrorKind
	// text is exactly what clap prints, trailing newline included.
	text string
}

func (e *clapError) Error() string { return e.text }

// exitCode is clap's status for the outcome: 0 for help and version, 2 for
// a usage error (and for help shown in place of a missing subcommand).
func (e *clapError) exitCode() int {
	switch e.kind {
	case clapErrorDisplayHelp, clapErrorDisplayVersion:
		return 0
	}
	return 2
}

// print writes the text where clap writes it, help and version to standard
// output and everything else to standard error, and returns the status
// (clap's Error::exit, without the exit).
func (e *clapError) print() int {
	var out io.Writer = os.Stderr
	if e.exitCode() == 0 {
		out = os.Stdout
	}
	_, _ = io.WriteString(out, e.text)
	return e.exitCode()
}

// parseCli is clap's Cli::try_parse_from: args[0] is the program name.
// Help, version and usage errors come back as *clapError.
func parseCli(args []string) (*cli, error) {
	parsed := &cli{}
	root := newCliCommand(parsed)
	// clap names the program in usage lines after the file it was run as
	// (`hel` in a worker root), keeping its own name only without one.
	bin := cliBinaryName
	if len(args) > 0 {
		if name, ok := pathFileName(args[0]); ok {
			bin = name
		}
		args = args[1:]
	}
	leaf, err := parseClapLevel(root, bin, args, true)
	if err != nil {
		return nil, err
	}
	leaf.Run(leaf, nil)
	return parsed, nil
}

// pathFileName is Rust's Path::file_name as clap reads argv[0]: the last
// normal component (Rust's components skip empty and `.` ones), absent for
// a path ending in `..` or without one, and only when it is UTF-8.
func pathFileName(path string) (string, bool) {
	separator := func(c rune) bool { return c < utf8.RuneSelf && os.IsPathSeparator(uint8(c)) }
	var last string
	for _, component := range strings.FieldsFunc(path, separator) {
		if component != "." {
			last = component
		}
	}
	if last == "" || last == ".." || !utf8.ValidString(last) {
		return "", false
	}
	return last, true
}

// Annotation keys: on commands, their declaration order; on flags, what
// clap knows about the option.
const (
	clapOrderKey     = "clap_order"
	clapIDKey        = "clap_id"
	clapValueNameKey = "clap_value_name"
	clapActionKey    = "clap_action"
	clapRequiredKey  = "clap_required"
	clapNonEmptyKey  = "clap_non_empty"
	clapDefaultKey   = "clap_default"
	clapPossibleKey  = "clap_possible_values"
	clapConflictsKey = "clap_conflicts_with"
	clapRequiresKey  = "clap_requires"
	// clapHelpCommandKey marks the `help` subcommand clap adds to every
	// command that has subcommands.
	clapHelpCommandKey = "clap_help_command"
)

// clapAction is how an option takes its values (clap's ArgAction).
type clapAction string

const (
	clapSetTrue clapAction = "set_true"
	clapSet     clapAction = "set"
	clapAppend  clapAction = "append"
	clapHelp    clapAction = "help"
	clapVersion clapAction = "version"
)

// clapArgSpec declares one option.
type clapArgSpec struct {
	// id is the field name, clap's argument id.
	id   string
	long string
	help string
	// valueName defaults to the id in upper snake case.
	valueName string
	action    clapAction
	required  bool
	hidden    bool
	// nonEmpty refuses an empty value, as clap's PathBuf parser does.
	nonEmpty       bool
	defaultValue   *string
	possibleValues []string
	conflictsWith  []string
	requires       []string
}

// clapArg is one option as the parser sees it: the spec plus its pflag
// flag, which holds the value. help and version have no flag.
type clapArg struct {
	clapArgSpec
	flag  *pflag.Flag
	order int
	short rune
}

func (a *clapArg) takesValue() bool { return a.action == clapSet || a.action == clapAppend }

// String is clap's Display for an argument: `--root <ROOT>`, `--kill`.
func (a *clapArg) String() string {
	text := "--" + a.long
	if a.takesValue() {
		text += " <" + a.valueName + ">"
	}
	return text
}

// addClapArg declares an option on flags, holding its value in value.
func addClapArg(flags *pflag.FlagSet, value pflag.Value, spec clapArgSpec) {
	flags.Var(value, spec.long, spec.help)
	set := func(key string, values ...string) {
		_ = flags.SetAnnotation(spec.long, key, values)
	}
	set(clapIDKey, spec.id)
	set(clapOrderKey, strconv.Itoa(flagIndex(flags, spec.long)))
	valueName := spec.valueName
	if valueName == "" {
		valueName = strings.ToUpper(strings.TrimLeft(spec.id, "_"))
	}
	set(clapValueNameKey, valueName)
	set(clapActionKey, string(spec.action))
	if spec.required {
		set(clapRequiredKey, "true")
	}
	if spec.nonEmpty {
		set(clapNonEmptyKey, "true")
	}
	if spec.defaultValue != nil {
		set(clapDefaultKey, *spec.defaultValue)
	}
	if spec.possibleValues != nil {
		set(clapPossibleKey, spec.possibleValues...)
	}
	if spec.conflictsWith != nil {
		set(clapConflictsKey, spec.conflictsWith...)
	}
	if spec.requires != nil {
		set(clapRequiresKey, spec.requires...)
	}
	if spec.hidden {
		_ = flags.MarkHidden(spec.long)
	}
}

// flagIndex is the declaration index of the flag named long in flags.
func flagIndex(flags *pflag.FlagSet, long string) int {
	index, found := 0, -1
	flags.VisitAll(func(flag *pflag.Flag) {
		if flag.Name == long {
			found = index
		}
		index++
	})
	return found
}

// clapArgOf reads an option's spec back from its flag.
func clapArgOf(flag *pflag.Flag) *clapArg {
	first := func(key string) (string, bool) {
		values, ok := flag.Annotations[key]
		if !ok || len(values) == 0 {
			return "", false
		}
		return values[0], true
	}
	arg := &clapArg{flag: flag}
	arg.long = flag.Name
	arg.help = flag.Usage
	arg.hidden = flag.Hidden
	arg.id, _ = first(clapIDKey)
	arg.valueName, _ = first(clapValueNameKey)
	action, _ := first(clapActionKey)
	arg.action = clapAction(action)
	_, arg.required = first(clapRequiredKey)
	_, arg.nonEmpty = first(clapNonEmptyKey)
	if value, ok := first(clapDefaultKey); ok {
		arg.defaultValue = &value
	}
	arg.possibleValues = flag.Annotations[clapPossibleKey]
	arg.conflictsWith = flag.Annotations[clapConflictsKey]
	arg.requires = flag.Annotations[clapRequiresKey]
	order, _ := first(clapOrderKey)
	arg.order, _ = strconv.Atoi(order)
	return arg
}

// newClapCommand is a subcommand with clap's about text.
func newClapCommand(name, about string, hidden bool) *cobra.Command {
	cmd := &cobra.Command{
		Use:         name,
		Short:       about,
		Hidden:      hidden,
		Annotations: map[string]string{},
	}
	cmd.Flags().SortFlags = false
	cmd.PersistentFlags().SortFlags = false
	return cmd
}

// addClapSubcommands adds subs to parent in declaration order, then the
// `help` subcommand clap adds last.
func addClapSubcommands(parent *cobra.Command, subs ...*cobra.Command) {
	for index, sub := range subs {
		sub.Annotations[clapOrderKey] = strconv.Itoa(index)
		parent.AddCommand(sub)
	}
	help := newClapCommand("help", "Print this message or the help of the given subcommand(s)", false)
	help.Annotations[clapOrderKey] = strconv.Itoa(len(subs))
	help.Annotations[clapHelpCommandKey] = "true"
	parent.AddCommand(help)
}

// Value types for the option kinds the command line uses.

// clapStringValue is a String or PathBuf option.
type clapStringValue struct{ target *string }

func (v clapStringValue) String() string        { return *v.target }
func (v clapStringValue) Set(text string) error { *v.target = text; return nil }
func (v clapStringValue) Type() string          { return "string" }

// clapStringsValue is a repeatable `Vec<String>` or `Vec<PathBuf>` option.
type clapStringsValue struct{ target *[]string }

func (v clapStringsValue) String() string { return strings.Join(*v.target, ",") }
func (v clapStringsValue) Set(text string) error {
	*v.target = append(*v.target, text)
	return nil
}
func (v clapStringsValue) Type() string { return "strings" }

// clapTrueValue is a SetTrue flag.
type clapTrueValue struct{ target *bool }

func (v clapTrueValue) String() string { return strconv.FormatBool(*v.target) }
func (v clapTrueValue) Set(string) error {
	*v.target = true
	return nil
}
func (v clapTrueValue) Type() string { return "bool" }

// clapBoolValue is a bool option that takes `true` or `false` as its value
// (clap's ArgAction::Set on a bool), as a pflag.Value.
type clapBoolValue struct{ target *bool }

func (v clapBoolValue) String() string { return fmt.Sprint(*v.target) }

func (v clapBoolValue) Set(text string) error {
	switch text {
	case "true":
		*v.target = true
	case "false":
		*v.target = false
	default:
		return errors.New("[possible values: true, false]")
	}
	return nil
}

func (v clapBoolValue) Type() string { return "bool" }

// clapOptionValue is an `Option<T>` option, absent until given and then
// read by parse (clap's value parser for T), as a pflag.Value.
type clapOptionValue[T any] struct {
	target **T
	parse  func(string) (T, error)
}

func (v clapOptionValue[T]) String() string {
	if *v.target == nil {
		return ""
	}
	return fmt.Sprint(**v.target)
}

func (v clapOptionValue[T]) Set(text string) error {
	value, err := v.parse(text)
	if err != nil {
		return err
	}
	*v.target = &value
	return nil
}

func (v clapOptionValue[T]) Type() string { return "value" }

// clapParsedValue is an option of a plain type read by parse.
type clapParsedValue[T any] struct {
	target *T
	parse  func(string) (T, error)
}

func (v clapParsedValue[T]) String() string { return fmt.Sprint(*v.target) }

func (v clapParsedValue[T]) Set(text string) error {
	value, err := v.parse(text)
	if err != nil {
		return err
	}
	*v.target = value
	return nil
}

func (v clapParsedValue[T]) Type() string { return "value" }

// clapChoiceValue is a string option limited to choices (clap's
// `value_parser = [...]`), as a pflag.Value.
type clapChoiceValue struct {
	target  *string
	choices []string
}

func (v clapChoiceValue) String() string { return *v.target }

func (v clapChoiceValue) Set(text string) error {
	if !slices.Contains(v.choices, text) {
		return fmt.Errorf("[possible values: %s]", strings.Join(v.choices, ", "))
	}
	*v.target = text
	return nil
}

func (v clapChoiceValue) Type() string { return "value" }

// Integer parsers with clap's errors: Rust's ParseIntError text, and
// RangedI64ValueParser's "<n> is not in <range>".

// rustParseInt is Rust's str::parse for an integer type: an optional sign
// (`+`, and `-` only when signed), then decimal digits.
func rustParseInt(text string, signed bool) (negative bool, magnitude uint64, err error) {
	digits := text
	if digits == "" {
		return false, 0, errors.New("cannot parse integer from empty string")
	}
	switch digits[0] {
	case '+':
		digits = digits[1:]
	case '-':
		if signed {
			negative = true
			digits = digits[1:]
		}
	}
	if digits == "" {
		return false, 0, errors.New("invalid digit found in string")
	}
	for _, c := range []byte(digits) {
		if c < '0' || c > '9' {
			return false, 0, errors.New("invalid digit found in string")
		}
	}
	value, parseErr := strconv.ParseUint(digits, 10, 64)
	if parseErr != nil {
		if negative {
			return false, 0, errors.New("number too small to fit in target type")
		}
		return false, 0, errors.New("number too large to fit in target type")
	}
	return negative, value, nil
}

// clapRangedI64 is clap's RangedI64ValueParser over [low, high].
func clapRangedI64(text string, low, high int64) (int64, error) {
	negative, magnitude, err := rustParseInt(text, true)
	if err != nil {
		return 0, err
	}
	var value int64
	switch {
	case !negative && magnitude > 1<<63-1:
		return 0, errors.New("number too large to fit in target type")
	case negative && magnitude > 1<<63:
		return 0, errors.New("number too small to fit in target type")
	case negative:
		// 1<<63 converts to math.MinInt64, which negation keeps.
		value = -int64(magnitude)
	default:
		value = int64(magnitude)
	}
	if value < low || value > high {
		return 0, fmt.Errorf("%d is not in %d..=%d", value, low, high)
	}
	return value, nil
}

func parseClapU32(text string) (uint32, error) {
	value, err := clapRangedI64(text, 0, 1<<32-1)
	return uint32(value), err
}

func parseClapI32(text string) (int32, error) {
	value, err := clapRangedI64(text, -1<<31, 1<<31-1)
	return int32(value), err
}

// parseClapU64 is clap's RangedU64ValueParser over all of u64, and the
// FromStr parser it uses for usize.
func parseClapU64(text string) (uint64, error) {
	_, magnitude, err := rustParseInt(text, false)
	return magnitude, err
}

// newCliCommand builds the command tree: the root with its version and its
// hidden global `--login-environment-ready` and `--worker-root-lock-fd`, the
// hidden `worker` command, and every worker subcommand with clap's names,
// hidden flags, defaults, value lists, conflicts and requirements. Running
// a leaf stores its parsed command in parsed.
func newCliCommand(parsed *cli) *cobra.Command {
	root := newClapCommand(cliBinaryName, cliAbout, false)
	root.Version = core.PackageVersion
	globals := root.PersistentFlags()
	addClapArg(globals, clapTrueValue{&parsed.loginEnvironmentReady}, clapArgSpec{
		id: "login_environment_ready", long: "login-environment-ready", action: clapSetTrue, hidden: true,
		help: "Internal handoff: the parent explicitly supplied the clean login snapshot",
	})
	addClapArg(globals, clapOptionValue[int32]{&parsed.workerRootLockFD, parseClapI32}, clapArgSpec{
		id: "worker_root_lock_fd", long: "worker-root-lock-fd", action: clapSet, hidden: true,
		help: "Descriptor transferred by login-environment re-exec",
	})

	workerCmd := newClapCommand("worker", "Internal target-side worker commands", true)
	addClapSubcommands(root, workerCmd)

	// leaf builds a worker subcommand whose run stores command().
	leaf := func(name, about string, hidden bool, declare func(flags *pflag.FlagSet), command func() workerCommand) *cobra.Command {
		cmd := newClapCommand(name, about, hidden)
		if declare != nil {
			declare(cmd.Flags())
		}
		cmd.Run = func(*cobra.Command, []string) {
			parsed.command = commandWorker{workerArgs{command: command()}}
		}
		return cmd
	}
	path := func(flags *pflag.FlagSet, target *string, id, help string) {
		addClapArg(flags, clapStringValue{target}, clapArgSpec{
			id: id, long: kebab(id), help: help, action: clapSet, required: true, nonEmpty: true,
		})
	}
	optionalPath := func(flags *pflag.FlagSet, target **string, id, help string) {
		addClapArg(flags, clapOptionValue[string]{target, parsePathValue}, clapArgSpec{
			id: id, long: kebab(id), help: help, action: clapSet, nonEmpty: true,
		})
	}
	text := func(flags *pflag.FlagSet, target *string, id, help string) {
		addClapArg(flags, clapStringValue{target}, clapArgSpec{
			id: id, long: kebab(id), help: help, action: clapSet, required: true,
		})
	}
	optionalText := func(flags *pflag.FlagSet, target **string, id, help string) {
		addClapArg(flags, clapOptionValue[string]{target, func(text string) (string, error) { return text, nil }}, clapArgSpec{
			id: id, long: kebab(id), help: help, action: clapSet,
		})
	}
	flag := func(flags *pflag.FlagSet, target *bool, id, help string, hidden bool) {
		addClapArg(flags, clapTrueValue{target}, clapArgSpec{
			id: id, long: kebab(id), help: help, action: clapSetTrue, hidden: hidden,
		})
	}
	repeated := func(flags *pflag.FlagSet, target *[]string, id, long, help string, nonEmpty bool) {
		addClapArg(flags, clapStringsValue{target}, clapArgSpec{
			id: id, long: long, help: help, action: clapAppend, nonEmpty: nonEmpty,
		})
	}

	authorizedKeys := newClapCommand("authorized-keys", "Locked `~/.ssh/authorized_keys` editor for direct Move transfers", true)
	{
		var install authorizedKeysCommandInstall
		installCmd := newClapCommand("install", "Install a restricted temporary Move key under an exclusive flock", false)
		path(installCmd.Flags(), &install.path, "path", "")
		text(installCmd.Flags(), &install.operationID, "operation_id", "")
		text(installCmd.Flags(), &install.from, "from", "")
		text(installCmd.Flags(), &install.command, "command", "")
		text(installCmd.Flags(), &install.publicKey, "public_key", "")
		installCmd.Run = func(*cobra.Command, []string) {
			parsed.command = commandWorker{workerArgs{command: workerCommandAuthorizedKeys{authorizedKeysArgs{command: install}}}}
		}
		var remove authorizedKeysCommandRemove
		removeCmd := newClapCommand("remove", "Remove a temporary Move key by its operation ID", false)
		path(removeCmd.Flags(), &remove.path, "path", "")
		text(removeCmd.Flags(), &remove.operationID, "operation_id", "")
		removeCmd.Run = func(*cobra.Command, []string) {
			parsed.command = commandWorker{workerArgs{command: workerCommandAuthorizedKeys{authorizedKeysArgs{command: remove}}}}
		}
		var reconcile authorizedKeysCommandReconcile
		reconcileCmd := newClapCommand("reconcile", "Remove stale `mj-move-*` keys not listed in `--keep-operation`", false)
		path(reconcileCmd.Flags(), &reconcile.path, "path", "")
		repeated(reconcileCmd.Flags(), &reconcile.keepOperations, "keep_operations", "keep-operation", "", false)
		reconcileCmd.Run = func(*cobra.Command, []string) {
			parsed.command = commandWorker{workerArgs{command: workerCommandAuthorizedKeys{authorizedKeysArgs{command: reconcile}}}}
		}
		addClapSubcommands(authorizedKeys, installCmd, removeCmd, reconcileCmd)
	}

	var exportMove workerCommandExportMoveStream
	var receiveMove workerCommandReceiveMoveStream
	var liveness workerCommandLiveness
	var holder workerCommandHolder
	var stop workerCommandStop
	var quiesce workerCommandQuiesce
	var identityAlive workerCommandIdentityAlive
	var discoverConfig workerCommandDiscoverConfig
	var run workerCommandRun
	var prepareHarness workerCommandPrepareHarness
	var proxy workerCommandProxy
	var acpSupervisor workerCommandAcpSupervisor
	var exportCheckpoint workerCommandExportCheckpoint
	var suspendPreflight workerCommandSuspendPreflight
	var restoreCheckpoint workerCommandRestoreCheckpoint
	var discardQueued workerCommandDiscardQueuedPrompts
	var installResource workerCommandInstallResource
	var memoryMcp workerCommandMemoryMcp
	mailboxHook := workerCommandMailboxHook{format: mailboxHookFormats[0]}
	var mailboxRequest workerCommandMailboxRequest
	subagentMcp := workerCommandSubagentMcp{
		role:                  core.SubagentMcpRoleParent,
		agentMailboxesEnabled: true,
		subtleSubagents:       true,
	}
	var diff workerCommandDiff
	var readFile workerCommandReadFile
	var fetch workerCommandFetch
	var storeFetch workerCommandStoreFetch
	var writeFile workerCommandWriteFile
	var pushBranch workerCommandPushBranch

	addClapSubcommands(workerCmd,
		authorizedKeys,
		leaf("export-move-stream", "Write a framed Move stream (archive + optional `.mj/keep/` tar) to stdout", true,
			func(flags *pflag.FlagSet) {
				path(flags, &exportMove.archive, "archive", "")
				optionalPath(flags, &exportMove.keepDir, "keep_dir", "")
				repeated(flags, &exportMove.skip, "skip", "skip", "", true)
			}, func() workerCommand { return exportMove }),
		leaf("receive-move-stream", "Receive and verify a framed Move stream from stdin", true,
			func(flags *pflag.FlagSet) {
				path(flags, &receiveMove.archive, "archive", "")
				text(flags, &receiveMove.expectedSHA256, "expected_sha256", "")
				optionalPath(flags, &receiveMove.keepDir, "keep_dir", "")
			}, func() workerCommand { return receiveMove }),
		leaf("liveness", "Print `alive`, `starting` or `dead` for the worker that owns a root, decided by the lock on its `worker.lock`", true,
			func(flags *pflag.FlagSet) { path(flags, &liveness.root, "root", "") },
			func() workerCommand { return liveness }),
		leaf("holder", "Print the pid of the worker holding a root's lock, or nothing", true,
			func(flags *pflag.FlagSet) { path(flags, &holder.root, "root", "") },
			func() workerCommand { return holder }),
		leaf("stop", "Stop the worker that owns a root and every process it started, and return only when its lock is free and none of them is left", true,
			func(flags *pflag.FlagSet) {
				path(flags, &stop.root, "root", "")
				flag(flags, &stop.kill, "kill", "Kill at once instead of allowing the TERM grace", false)
			}, func() workerCommand { return stop }),
		leaf("quiesce", "Ask a live worker to stop every process for a managed suspend", true,
			func(flags *pflag.FlagSet) {
				path(flags, &quiesce.root, "root", "")
				text(flags, &quiesce.operation, "operation", "")
				path(flags, &quiesce.checkout, "checkout", "")
				repeated(flags, &quiesce.childSessions, "child_sessions", "child-session", "", false)
			}, func() workerCommand { return quiesce }),
		leaf("identity-alive", "Print `alive` or `dead` for a recorded worker process identity in this target's PID namespace", true,
			func(flags *pflag.FlagSet) {
				addClapArg(flags, clapParsedValue[uint32]{&identityAlive.pid, parseClapU32}, clapArgSpec{
					id: "pid", long: "pid", action: clapSet, required: true,
				})
				addClapArg(flags, clapParsedValue[uint64]{&identityAlive.start, parseClapU64}, clapArgSpec{
					id: "start", long: "start", action: clapSet, required: true,
				})
			}, func() workerCommand { return identityAlive }),
		leaf("build-id", "Print this worker's compatibility identity. The controller asks an image's worker this before linking a session to it", true,
			nil, func() workerCommand { return workerCommandBuildID{} }),
		leaf("discover-config", "Discover profile model choices without submitting a prompt", false,
			func(flags *pflag.FlagSet) { path(flags, &discoverConfig.spec, "spec", "") },
			func() workerCommand { return discoverConfig }),
		leaf("run", "Own an ACP bridge and durable session event log", false,
			func(flags *pflag.FlagSet) {
				path(flags, &run.root, "root", "")
				path(flags, &run.config, "config", "")
			}, func() workerCommand { return run }),
		leaf("prepare-harness", "Prepare an exact managed harness without starting a session worker", false,
			func(flags *pflag.FlagSet) { path(flags, &prepareHarness.config, "config", "") },
			func() workerCommand { return prepareHarness }),
		leaf("proxy", "Proxy JSON-lines between stdio and a detached worker", false,
			func(flags *pflag.FlagSet) { path(flags, &proxy.root, "root", "") },
			func() workerCommand { return proxy }),
		leaf("acp-supervisor", "Supervise the ACP bridge process tree for a worker daemon", false,
			func(flags *pflag.FlagSet) { path(flags, &acpSupervisor.spec, "spec", "") },
			func() workerCommand { return acpSupervisor }),
		leaf("export-checkpoint", "Build a target-side archive for verified controller transfer", false,
			func(flags *pflag.FlagSet) {
				path(flags, &exportCheckpoint.spec, "spec", "Export specification path, or `-` to read it from standard input")
			}, func() workerCommand { return exportCheckpoint }),
		leaf("suspend-preflight", "Run read-only workspace inventory and optional dry-run size estimate on the target", false,
			func(flags *pflag.FlagSet) {
				path(flags, &suspendPreflight.spec, "spec", "Preflight specification path, or `-` to read it from standard input")
			}, func() workerCommand { return suspendPreflight }),
		leaf("capture-checkpoint", "Seal target-owned checkpoint inputs while ACP dispatch is frozen", true,
			nil, func() workerCommand { return workerCommandCaptureCheckpoint{} }),
		leaf("pack-checkpoint", "Package a sealed checkpoint after ACP dispatch has resumed", true,
			nil, func() workerCommand { return workerCommandPackCheckpoint{} }),
		leaf("restore-checkpoint", "Restore a verified archive into a freshly cloned target", false,
			func(flags *pflag.FlagSet) {
				path(flags, &restoreCheckpoint.spec, "spec", "")
				addClapArg(flags, clapTrueValue{&restoreCheckpoint.missingParent}, clapArgSpec{
					id: "missing_parent", long: "missing-parent", action: clapSetTrue,
					help:          "Restore only missing parent state; never modify checkout or child reports",
					conflictsWith: []string{"missing_checkout"},
				})
				flag(flags, &restoreCheckpoint.missingCheckout, "missing_checkout", "", false)
				addClapArg(flags, clapOptionValue[string]{&restoreCheckpoint.relaySeed, parsePathValue}, clapArgSpec{
					id: "relay_seed", long: "relay-seed", action: clapSet, nonEmpty: true,
					help:     "Current controller-owned queue/frontier, for partial parent recovery",
					requires: []string{"missing_parent"},
				})
			}, func() workerCommand { return restoreCheckpoint }),
		leaf("discard-queued-prompts", "Journal queue discard while the worker is stopped, before any dispatch", true,
			func(flags *pflag.FlagSet) {
				path(flags, &discardQueued.root, "root", "")
				text(flags, &discardQueued.sessionID, "session_id", "")
				text(flags, &discardQueued.commandID, "command_id", "")
			}, func() workerCommand { return discardQueued }),
		leaf("install-resource", "Install one streamed resource directory on a remote target", false,
			func(flags *pflag.FlagSet) { path(flags, &installResource.destination, "destination", "") },
			func() workerCommand { return installResource }),
		leaf("memory-mcp", "Serve project history tools over MCP stdio", false,
			func(flags *pflag.FlagSet) {
				addClapArg(flags, clapOptionValue[string]{&memoryMcp.root, parsePathValue}, clapArgSpec{
					id: "_root", long: "root", action: clapSet, hidden: true, nonEmpty: true,
					help: "Accepted for staged Kimi profiles created by older controllers",
				})
				optionalPath(flags, &memoryMcp.historySocket, "history_socket", "")
			}, func() workerCommand { return memoryMcp }),
		leaf("mailbox-hook", "Drain mailbox events for one harness tool hook", false,
			func(flags *pflag.FlagSet) {
				path(flags, &mailboxHook.socket, "socket", "")
				addClapArg(flags, clapChoiceValue{&mailboxHook.event, mailboxHookEvents}, clapArgSpec{
					id: "event", long: "event", action: clapSet, required: true, possibleValues: mailboxHookEvents,
				})
				addClapArg(flags, clapChoiceValue{&mailboxHook.format, mailboxHookFormats}, clapArgSpec{
					id: "format", long: "format", action: clapSet, possibleValues: mailboxHookFormats,
					defaultValue: &mailboxHookFormats[0],
				})
			}, func() workerCommand { return mailboxHook }),
		leaf("mailbox-request", "Exchange one mailbox lease request for a durable harness inbox", false,
			func(flags *pflag.FlagSet) { path(flags, &mailboxRequest.socket, "socket", "") },
			func() workerCommand { return mailboxRequest }),
		leaf("subagent-mcp", "Serve Mjolnir-owned delegation tools over MCP stdio", false,
			func(flags *pflag.FlagSet) {
				path(flags, &subagentMcp.socket, "socket", "")
				addClapArg(flags, clapOptionValue[core.HarnessKind]{&subagentMcp.harness, core.ParseHarnessKind}, clapArgSpec{
					id: "harness", long: "harness", action: clapSet,
					help: "Harness used to choose the MCP server instructions",
				})
				parent := core.SubagentMcpRoleParent.String()
				addClapArg(flags, clapParsedValue[core.SubagentMcpRole]{&subagentMcp.role, core.ParseSubagentMcpRole}, clapArgSpec{
					id: "role", long: "role", action: clapSet, defaultValue: &parent,
					help: "`parent` serves the delegation tools; `child` serves only `handback`, a child's report to the session that started it",
				})
				enabled := "true"
				addClapArg(flags, clapBoolValue{&subagentMcp.agentMailboxesEnabled}, clapArgSpec{
					id: "agent_mailboxes_enabled", long: "agent-mailboxes-enabled", action: clapSet, hidden: true,
					defaultValue: &enabled, possibleValues: []string{"true", "false"},
					help: "Accepted for older launch arguments",
				})
				addClapArg(flags, clapBoolValue{&subagentMcp.subtleSubagents}, clapArgSpec{
					id: "subtle_subagents", long: "subtle-subagents", action: clapSet,
					defaultValue: &enabled, possibleValues: []string{"true", "false"},
					help: "False when only the straightforward task type is configured: the spawn_worker tool then offers no task type",
				})
				flag(flags, &subagentMcp.reviewGate, "review_gate",
					"Accepted for configurations written by older workers; the review tools now belong to the `--review-server` server", true)
				flag(flags, &subagentMcp.reviewServer, "review_server",
					"Serve the before-push review tools as `mj-review` instead of the delegation tools", false)
			}, func() workerCommand { return subagentMcp }),
		leaf("diff", "Print a unified diff of the session's work in one repository", false,
			func(flags *pflag.FlagSet) {
				path(flags, &diff.repository, "repository", "")
				optionalText(flags, &diff.base, "base", "The commit the session started from, when the controller recorded one")
				optionalText(flags, &diff.branch, "branch", "The session branch, whose reflog names the commit it was created at")
				flag(flags, &diff.json, "json", "Include the resolved base and HEAD commit in JSON output", false)
			}, func() workerCommand { return diff }),
		leaf("read-file", "Write one file from the session workspace to standard output", false,
			func(flags *pflag.FlagSet) {
				path(flags, &readFile.root, "root", "")
				path(flags, &readFile.path, "path", "Path relative to the workspace root")
			}, func() workerCommand { return readFile }),
		leaf("fetch", "Write a file, a zip archive of a directory, or a JSON listing of either to standard output, after one JSON line naming which it is and where it resolved", false,
			func(flags *pflag.FlagSet) {
				path(flags, &fetch.path, "path", "The file or directory to fetch")
				repeated(flags, &fetch.roots, "roots", "root", "A directory the path may resolve inside; repeatable", true)
				flag(flags, &fetch.temp, "temp", "Also allow this machine's temporary directory", false)
				flag(flags, &fetch.list, "list", "List the path instead of reading it", false)
			}, func() workerCommand { return fetch }),
		leaf("store-fetch", "Save fetched bytes in this target's temporary directory", false,
			func(flags *pflag.FlagSet) {
				text(flags, &storeFetch.session, "session", "")
				text(flags, &storeFetch.source, "source", "")
				flag(flags, &storeFetch.directory, "directory", "", false)
				addClapArg(flags, clapParsedValue[uint64]{&storeFetch.length, parseClapU64}, clapArgSpec{
					id: "length", long: "length", action: clapSet, required: true,
				})
			}, func() workerCommand { return storeFetch }),
		leaf("write-file", "Atomically publish stdin as a file in the session workspace", false,
			func(flags *pflag.FlagSet) {
				path(flags, &writeFile.root, "root", "")
				path(flags, &writeFile.path, "path", "")
				flag(flags, &writeFile.overwrite, "overwrite", "", false)
				addClapArg(flags, clapOptionValue[uint64]{&writeFile.length, parseClapU64}, clapArgSpec{
					id: "length", long: "length", action: clapSet,
					help: "Expected input length; when present, premature EOF or trailing bytes must not publish a partial file",
				})
			}, func() workerCommand { return writeFile }),
		leaf("push-branch", "Push the repository's current HEAD to a branch on its push remote", false,
			func(flags *pflag.FlagSet) {
				path(flags, &pushBranch.root, "root", "Worker root containing the session's existing Git authentication")
				path(flags, &pushBranch.repository, "repository", "")
				text(flags, &pushBranch.branch, "branch", "")
			}, func() workerCommand { return pushBranch }),
	)
	return root
}

// parsePathValue is clap's PathBuf parser after its empty check.
func parsePathValue(text string) (string, error) { return text, nil }

// kebab is clap's derived long name for a field: `operation_id` is
// `operation-id`.
func kebab(id string) string {
	return strings.ReplaceAll(strings.TrimLeft(id, "_"), "_", "-")
}

// The parser.

// clapLevel is one command's parse: its options and what was given for it.
type clapLevel struct {
	cmd    *cobra.Command
	bin    string
	isRoot bool
	args   []*clapArg
	// matched are the options given explicitly, in the order first given.
	matched []*clapArg
	// pending is the option whose value the next argument is; its values so
	// far are pendingValues, and pendingDone once it has all of them.
	pending       *clapArg
	pendingValues []string
	pendingDone   bool
}

func newClapLevel(cmd *cobra.Command, bin string, isRoot bool) *clapLevel {
	level := &clapLevel{cmd: cmd, bin: bin, isRoot: isRoot}
	cmd.LocalFlags().VisitAll(func(flag *pflag.Flag) {
		level.args = append(level.args, clapArgOf(flag))
	})
	// Global options are propagated to every subcommand after its own.
	cmd.InheritedFlags().VisitAll(func(flag *pflag.Flag) {
		arg := clapArgOf(flag)
		arg.order = len(level.args)
		level.args = append(level.args, arg)
	})
	slices.SortStableFunc(level.args, func(a, b *clapArg) int { return cmp.Compare(a.order, b.order) })
	level.args = append(level.args, &clapArg{
		clapArgSpec: clapArgSpec{id: "help", long: "help", help: "Print help", action: clapHelp},
		order:       1 << 30, short: 'h',
	})
	if isRoot && cmd.Version != "" {
		level.args = append(level.args, &clapArg{
			clapArgSpec: clapArgSpec{id: "version", long: "version", help: "Print version", action: clapVersion},
			order:       1<<30 + 1, short: 'V',
		})
	}
	return level
}

func (l *clapLevel) findLong(name string) *clapArg {
	for _, arg := range l.args {
		if arg.long == name {
			return arg
		}
	}
	return nil
}

func (l *clapLevel) findShort(short rune) *clapArg {
	for _, arg := range l.args {
		if arg.short != 0 && arg.short == short {
			return arg
		}
	}
	return nil
}

func (l *clapLevel) findID(id string) *clapArg {
	for _, arg := range l.args {
		if arg.id == id {
			return arg
		}
	}
	return nil
}

func (l *clapLevel) explicit(id string) bool {
	return slices.ContainsFunc(l.matched, func(arg *clapArg) bool { return arg.id == id })
}

// parseClapLevel parses args for cmd and, when a subcommand follows, for it,
// returning the leaf that was chosen.
func parseClapLevel(cmd *cobra.Command, bin string, args []string, isRoot bool) (*cobra.Command, error) {
	level := newClapLevel(cmd, bin, isRoot)
	trailing := false
	var sub *cobra.Command
	var rest []string
	for index := 0; index < len(args); index++ {
		arg := args[index]
		if !trailing {
			if found := findClapSubcommand(cmd, arg); found != nil {
				if isClapHelpCommand(found) {
					return nil, level.helpSubcommand(args[index+1:])
				}
				sub, rest = found, args[index+1:]
				break
			}
			if arg == "--" {
				trailing = true
				continue
			}
			if long, ok := strings.CutPrefix(arg, "--"); ok {
				name, value, hasValue := strings.Cut(long, "=")
				option := level.findLong(name)
				if option == nil {
					_ = level.resolvePending()
					return nil, level.unknownLongError(name, args[index+1:])
				}
				switch {
				case option.takesValue() && hasValue:
					if err := level.react(option, []string{value}); err != nil {
						return nil, err
					}
				case option.takesValue():
					if err := level.resolvePending(); err != nil {
						return nil, err
					}
					level.pending, level.pendingValues, level.pendingDone = option, nil, false
				case hasValue:
					_ = level.resolvePending()
					return nil, level.tooManyValuesError(value, option)
				default:
					if err := level.react(option, nil); err != nil {
						return nil, err
					}
				}
				continue
			}
			if len(arg) > 1 && arg[0] == '-' {
				for _, short := range arg[1:] {
					option := level.findShort(short)
					if option == nil {
						_ = level.resolvePending()
						return nil, level.usageError(clapRendered{
							message: fmt.Sprintf("unexpected argument '-%c' found", short),
							usage:   level.usage(nil, nil),
						})
					}
					if err := level.react(option, nil); err != nil {
						return nil, err
					}
				}
				continue
			}
			if level.pending != nil && !level.pendingDone {
				level.pendingValues = append(level.pendingValues, arg)
				level.pendingDone = true
				continue
			}
		}
		_ = level.resolvePending()
		return nil, level.matchArgError(arg, trailing)
	}
	leaf := cmd
	if sub != nil {
		chosen, err := parseClapLevel(sub, bin+" "+sub.Name(), rest, false)
		if err != nil {
			return nil, err
		}
		leaf = chosen
	}
	if err := level.resolvePending(); err != nil {
		return nil, err
	}
	if err := level.validate(sub != nil); err != nil {
		return nil, err
	}
	return leaf, nil
}

// findClapSubcommand is clap's find_subcommand: a subcommand by name.
func findClapSubcommand(cmd *cobra.Command, name string) *cobra.Command {
	for _, sub := range cmd.Commands() {
		if sub.Name() == name {
			return sub
		}
	}
	return nil
}

func isClapHelpCommand(cmd *cobra.Command) bool {
	return cmd.Annotations[clapHelpCommandKey] != ""
}

// clapSubcommands are cmd's subcommands in declaration order, `help` last.
func clapSubcommands(cmd *cobra.Command) []*cobra.Command {
	subs := slices.Clone(cmd.Commands())
	slices.SortStableFunc(subs, func(a, b *cobra.Command) int {
		orderA, _ := strconv.Atoi(a.Annotations[clapOrderKey])
		orderB, _ := strconv.Atoi(b.Annotations[clapOrderKey])
		return cmp.Compare(orderA, orderB)
	})
	return subs
}

// hasVisibleSubcommands is clap's has_visible_subcommands: a subcommand
// other than `help` that is not hidden.
func hasVisibleSubcommands(cmd *cobra.Command) bool {
	for _, sub := range cmd.Commands() {
		if !sub.Hidden && !isClapHelpCommand(sub) {
			return true
		}
	}
	return false
}

// resolvePending gives the pending option the values it collected.
func (l *clapLevel) resolvePending() error {
	if l.pending == nil {
		return nil
	}
	option, values := l.pending, l.pendingValues
	l.pending, l.pendingValues, l.pendingDone = nil, nil, false
	return l.react(option, values)
}

// react is clap's Parser::react: what an option does once its values are
// known.
func (l *clapLevel) react(option *clapArg, values []string) error {
	if err := l.resolvePending(); err != nil {
		return err
	}
	switch option.action {
	case clapHelp:
		return &clapError{kind: clapErrorDisplayHelp, text: clapHelpText(l.cmd, l.bin)}
	case clapVersion:
		return &clapError{kind: clapErrorDisplayVersion, text: cliBinaryName + " " + l.cmd.Version + "\n"}
	}
	if option.takesValue() && len(values) == 0 {
		return l.emptyValueError(option)
	}
	if option.action != clapAppend && l.explicit(option.id) {
		return l.usageError(clapRendered{
			message: fmt.Sprintf("the argument '%s' cannot be used multiple times", option),
			usage:   l.usage(nil, nil),
		})
	}
	if !l.explicit(option.id) {
		l.matched = append(l.matched, option)
	}
	if option.action == clapSetTrue {
		return option.flag.Value.Set("true")
	}
	for _, value := range values {
		if value == "" && (option.nonEmpty || option.possibleValues != nil) {
			return l.emptyValueError(option)
		}
		if option.possibleValues != nil && !slices.Contains(option.possibleValues, value) {
			return l.invalidValueError(option, value)
		}
		if err := option.flag.Value.Set(value); err != nil {
			return l.usageError(clapRendered{
				message: fmt.Sprintf("invalid value '%s' for '%s': %v", value, option, err),
			})
		}
	}
	return nil
}

// validate is clap's Validator::validate for this level.
func (l *clapLevel) validate(hasSubcommand bool) error {
	if !hasSubcommand && l.cmd.HasSubCommands() {
		// Every command here with subcommands requires one, and clap's
		// derive then also shows help when nothing at all was given.
		if len(l.matched) == 0 {
			return &clapError{kind: clapErrorHelpOnMissingSubcommand, text: clapHelpText(l.cmd, l.bin)}
		}
		var names []string
		for _, sub := range clapSubcommands(l.cmd) {
			names = append(names, sub.Name())
		}
		return l.usageError(clapRendered{
			message: fmt.Sprintf("'%s' requires a subcommand but one was not provided\n  [subcommands: %s]",
				l.bin, strings.Join(names, ", ")),
			usage: l.usage(l.requiredGraph(), nil),
		})
	}
	// Conflicts.
	for _, option := range l.matched {
		conflicts := l.gatherConflicts(option.id)
		if len(conflicts) == 0 {
			continue
		}
		var others []string
		for _, id := range conflicts {
			if text := l.findID(id).String(); !slices.Contains(others, text) {
				others = append(others, text)
			}
		}
		message := fmt.Sprintf("the argument '%s' cannot be used with", option)
		if len(others) == 1 {
			message += fmt.Sprintf(" '%s'", others[0])
		} else {
			message += ":"
			for _, other := range others {
				message += "\n  " + other
			}
		}
		var used []string
		for _, given := range l.matched {
			if !given.hidden && !slices.Contains(conflicts, given.id) {
				used = append(used, given.id)
			}
		}
		var required []string
		for _, id := range used {
			for _, requirement := range l.findID(id).requires {
				if !slices.Contains(used, requirement) && !slices.Contains(conflicts, requirement) {
					required = append(required, requirement)
				}
			}
		}
		required = append(required, used...)
		return l.usageError(clapRendered{message: message, usage: l.usage(l.requiredGraph(), required)})
	}
	// Requirements.
	required := l.requiredGraph()
	for _, option := range l.matched {
		for _, requirement := range option.requires {
			if !slices.Contains(required, requirement) {
				required = append(required, requirement)
			}
		}
	}
	var missing []string
	for _, id := range required {
		if !l.explicit(id) && len(l.gatherConflicts(id)) == 0 {
			missing = append(missing, id)
		}
	}
	if len(missing) == 0 {
		return nil
	}
	var listed []string
	for _, id := range append(slices.Clone(required), missing...) {
		if l.explicit(id) {
			continue
		}
		if text := l.findID(id).String(); !slices.Contains(listed, text) {
			listed = append(listed, text)
		}
	}
	var used []string
	for _, given := range l.matched {
		if !given.hidden {
			used = append(used, given.id)
		}
	}
	used = append(used, missing...)
	return l.usageError(clapRendered{
		message: "the following required arguments were not provided:\n  " + strings.Join(listed, "\n  "),
		usage:   l.usage(required, used),
	})
}

// requiredGraph is the ids of the level's required options, in declaration
// order.
func (l *clapLevel) requiredGraph() []string {
	var required []string
	for _, option := range l.args {
		if option.required {
			required = append(required, option.id)
		}
	}
	return required
}

// gatherConflicts is clap's Conflicts::gather_conflicts: the given options
// that conflict with id, in either direction.
func (l *clapLevel) gatherConflicts(id string) []string {
	var direct []string
	if option := l.findID(id); option != nil {
		direct = option.conflictsWith
	}
	var conflicts []string
	for _, other := range l.matched {
		if other.id == id {
			continue
		}
		if slices.Contains(direct, other.id) {
			conflicts = append(conflicts, other.id)
		}
		if slices.Contains(other.conflictsWith, id) {
			conflicts = append(conflicts, other.id)
		}
	}
	return conflicts
}

// helpSubcommand is clap's `help [COMMAND]...`: the help of the named
// subcommand, or an error naming the first that does not exist.
func (l *clapLevel) helpSubcommand(names []string) error {
	cmd, bin := l.cmd, l.bin
	for _, name := range names {
		sub := findClapSubcommand(cmd, name)
		if sub == nil {
			level := newClapLevel(cmd, bin, cmd == l.cmd && l.isRoot)
			return level.usageError(clapRendered{
				message: fmt.Sprintf("unrecognized subcommand '%s'", name),
				usage:   level.usage(nil, nil),
			})
		}
		cmd, bin = sub, bin+" "+name
	}
	return &clapError{kind: clapErrorDisplayHelp, text: clapHelpText(cmd, bin)}
}

// unknownLongError is clap's did_you_mean_error for `--name`.
func (l *clapLevel) unknownLongError(name string, remaining []string) error {
	rendered := clapRendered{message: fmt.Sprintf("unexpected argument '--%s' found", name)}
	var longs []string
	for _, option := range l.args {
		longs = append(longs, option.long)
	}
	var usedIDs []string
	if candidates := didYouMean(name, longs); len(candidates) > 0 {
		suggestion := candidates[len(candidates)-1]
		rendered.suggestedArg = "--" + suggestion
		// clap adds the suggestion to the matches to build its usage.
		if option := l.findLong(suggestion); option != nil && !l.explicit(option.id) {
			usedIDs = append(usedIDs, option.id)
		}
	} else {
		// A long option of a subcommand that is named later.
		best := -1
		for _, sub := range l.cmd.Commands() {
			if isClapHelpCommand(sub) {
				continue
			}
			level := newClapLevel(sub, l.bin+" "+sub.Name(), false)
			var subLongs []string
			for _, option := range level.args {
				subLongs = append(subLongs, option.long)
			}
			candidates := didYouMean(name, subLongs)
			if len(candidates) == 0 {
				continue
			}
			position := slices.Index(remaining, sub.Name())
			if position < 0 || best >= 0 && position >= best {
				continue
			}
			best = position
			rendered.suggestions = []string{fmt.Sprintf("'%s --%s' exists", sub.Name(), candidates[len(candidates)-1])}
		}
	}
	var used []string
	for _, given := range l.matched {
		if !given.hidden {
			used = append(used, given.id)
		}
	}
	used = append(used, usedIDs...)
	rendered.usage = l.usage(l.requiredGraph(), used)
	return l.usageError(rendered)
}

// tooManyValuesError is clap's error for `--flag=value` on an option that
// takes none.
func (l *clapLevel) tooManyValuesError(value string, option *clapArg) error {
	required := l.requiredGraph()
	var used []string
	for _, given := range l.matched {
		if !given.hidden && !slices.Contains(required, given.id) {
			used = append(used, given.id)
		}
	}
	used = append(used, option.id)
	return l.usageError(clapRendered{
		message: fmt.Sprintf("unexpected value '%s' for '%s' found; no more were expected", value, option),
		usage:   l.usage(nil, used),
	})
}

// emptyValueError is clap's error for an option given without its value.
func (l *clapLevel) emptyValueError(option *clapArg) error {
	message := fmt.Sprintf("a value is required for '%s' but none was supplied", option)
	if len(option.possibleValues) > 0 {
		message += "\n  [possible values: " + strings.Join(option.possibleValues, ", ") + "]"
	}
	return l.usageError(clapRendered{message: message})
}

// invalidValueError is clap's error for a value outside an option's list.
func (l *clapLevel) invalidValueError(option *clapArg, value string) error {
	rendered := clapRendered{
		message: fmt.Sprintf("invalid value '%s' for '%s'\n  [possible values: %s]",
			value, option, strings.Join(option.possibleValues, ", ")),
	}
	if candidates := didYouMean(value, option.possibleValues); len(candidates) > 0 {
		rendered.suggestedValue = candidates[len(candidates)-1]
	}
	return l.usageError(rendered)
}

// matchArgError is clap's error for an argument that is neither an option
// nor a subcommand here.
func (l *clapLevel) matchArgError(arg string, trailing bool) error {
	if trailing && findClapSubcommand(l.cmd, arg) != nil {
		return l.usageError(clapRendered{
			message:     fmt.Sprintf("unexpected argument '%s' found", arg),
			suggestions: []string{fmt.Sprintf("subcommand '%s' exists; to use it, remove the '--' before it", arg)},
			usage:       l.usage(nil, nil),
		})
	}
	if l.cmd.HasSubCommands() {
		var names []string
		for _, sub := range clapSubcommands(l.cmd) {
			names = append(names, sub.Name())
		}
		if candidates := didYouMean(arg, names); len(candidates) > 0 {
			return l.usageError(clapRendered{
				message:              fmt.Sprintf("unrecognized subcommand '%s'", arg),
				suggestedSubcommands: candidates,
				suggestions:          []string{},
				usage:                l.usage(nil, nil),
			})
		}
		return l.usageError(clapRendered{
			message: fmt.Sprintf("unrecognized subcommand '%s'", arg),
			usage:   l.usage(nil, nil),
		})
	}
	return l.usageError(clapRendered{
		message: fmt.Sprintf("unexpected argument '%s' found", arg),
		usage:   l.usage(nil, nil),
	})
}

// clapRendered is the context of a usage error (clap's RichFormatter
// inputs).
type clapRendered struct {
	message              string
	suggestedSubcommands []string
	suggestedArg         string
	suggestedValue       string
	// suggestions are clap's `tip:` lines; non-nil even when empty where
	// clap records the context.
	suggestions []string
	usage       string
}

// usageError renders r as clap's RichFormatter does.
func (l *clapLevel) usageError(r clapRendered) error {
	var b strings.Builder
	b.WriteString("error: " + r.message)
	suggested := false
	tip := func() {
		b.WriteString("\n")
		if !suggested {
			b.WriteString("\n")
			suggested = true
		}
	}
	if r.suggestedSubcommands != nil {
		tip()
		if len(r.suggestedSubcommands) == 1 {
			b.WriteString("  tip: a similar subcommand exists: ")
		} else {
			b.WriteString("  tip: some similar subcommands exist: ")
		}
		for index, name := range r.suggestedSubcommands {
			if index > 0 {
				b.WriteString(", ")
			}
			b.WriteString("'" + name + "'")
		}
	}
	if r.suggestedArg != "" {
		tip()
		b.WriteString("  tip: a similar argument exists: '" + r.suggestedArg + "'")
	}
	if r.suggestedValue != "" {
		tip()
		b.WriteString("  tip: a similar value exists: '" + r.suggestedValue + "'")
	}
	if r.suggestions != nil {
		if !suggested {
			b.WriteString("\n")
		}
		for _, suggestion := range r.suggestions {
			b.WriteString("\n  tip: " + suggestion)
		}
	}
	if r.usage != "" {
		b.WriteString("\n\n" + r.usage)
	}
	b.WriteString("\n\nFor more information, try '--help'.\n")
	return &clapError{kind: clapErrorUsage, text: b.String()}
}

// usage is clap's usage line with its title. With used empty it is the help
// usage: required options, `[OPTIONS]` when other visible options exist,
// and `<COMMAND>` when there are visible subcommands. With used, it is
// clap's "smart" usage: required and used options, and `<COMMAND>` whenever
// a subcommand is required. required is the requirement graph to show;
// nil means the level's own.
func (l *clapLevel) usage(required []string, used []string) string {
	if required == nil {
		required = l.requiredGraph()
	}
	parts := []string{l.bin}
	if len(used) == 0 && l.needsOptionsTag() {
		parts = append(parts, "[OPTIONS]")
	}
	var shown []string
	for _, id := range append(slices.Clone(required), used...) {
		option := l.findID(id)
		if option == nil {
			continue
		}
		if text := option.String(); !slices.Contains(shown, text) {
			shown = append(shown, text)
		}
	}
	parts = append(parts, shown...)
	if len(used) == 0 {
		if hasVisibleSubcommands(l.cmd) {
			parts = append(parts, "<COMMAND>")
		}
	} else if l.cmd.HasSubCommands() {
		parts = append(parts, "<COMMAND>")
	}
	return "Usage: " + strings.Join(parts, " ")
}

// needsOptionsTag is clap's needs_options_tag: a visible, optional option
// other than help and version.
func (l *clapLevel) needsOptionsTag() bool {
	for _, option := range l.args {
		if option.action == clapHelp || option.action == clapVersion {
			continue
		}
		if !option.hidden && !option.required {
			return true
		}
	}
	return false
}

// clapHelpText is cmd's help in clap's layout: about, `Usage:` line,
// `Commands:` (visible subcommands and `help`), `Options:` with each
// option's value name, default and possible values, then `-h, --help` and,
// on the root, `-V, --version`. Hidden commands and options are left out.
func clapHelpText(cmd *cobra.Command, bin string) string {
	if isClapHelpCommand(cmd) {
		return cmd.Short + "\n\nUsage: " + bin + " [COMMAND]...\n\nArguments:\n  [COMMAND]...  Print help for the subcommand(s)\n"
	}
	level := newClapLevel(cmd, bin, cmd.Version != "")
	var b strings.Builder
	if cmd.Short != "" {
		b.WriteString(cmd.Short + "\n")
	}
	b.WriteString("\n" + level.usage(nil, nil) + "\n\n")
	first := true
	if hasVisibleSubcommands(cmd) {
		first = false
		b.WriteString("Commands:\n")
		var subs []*cobra.Command
		longest := 2
		for _, sub := range clapSubcommands(cmd) {
			if sub.Hidden {
				continue
			}
			subs = append(subs, sub)
			longest = max(longest, displayWidth(sub.Name()))
		}
		nextLine := false
		for _, sub := range subs {
			nextLine = nextLine || forceNextLineHelp(longest, displayWidth(sub.Short))
		}
		for index, sub := range subs {
			if index > 0 {
				b.WriteString("\n")
			}
			b.WriteString("  " + sub.Name())
			if nextLine {
				b.WriteString("\n          ")
			} else {
				b.WriteString(strings.Repeat(" ", longest+2-displayWidth(sub.Name())))
			}
			b.WriteString(sub.Short)
		}
	}
	var shown []*clapArg
	for _, option := range level.args {
		if !option.hidden {
			shown = append(shown, option)
		}
	}
	if len(shown) > 0 {
		if !first {
			b.WriteString("\n\n")
		}
		b.WriteString("Options:\n")
		longest := 2
		for _, option := range shown {
			longest = max(longest, displayWidth(option.String())+4)
		}
		nextLine := false
		for _, option := range shown {
			nextLine = nextLine || forceNextLineHelp(longest, displayWidth(option.help)+displayWidth(specValues(option)))
		}
		for index, option := range shown {
			if index > 0 {
				b.WriteString("\n")
			}
			b.WriteString("  ")
			if option.short != 0 {
				b.WriteString("-" + string(option.short) + ", ")
			} else {
				b.WriteString("    ")
			}
			b.WriteString(option.String())
			if nextLine {
				b.WriteString("\n          ")
			} else {
				b.WriteString(strings.Repeat(" ", longest+2-(displayWidth(option.String())+4)))
			}
			help := option.help
			if specs := specValues(option); specs != "" {
				if help != "" {
					help += " "
				}
				help += specs
			}
			b.WriteString(help)
		}
	}
	text := b.String()
	// clap's trim_start_lines, trim_end, and the final newline.
	if newline := strings.IndexByte(text, '\n'); newline >= 0 && strings.TrimSpace(text[:newline]) == "" {
		text = text[newline+1:]
	}
	return strings.TrimRight(text, " \t\n\r") + "\n"
}

// specValues is clap's bracketed tail of an option's help: its default and
// its possible values.
func specValues(option *clapArg) string {
	if !option.takesValue() {
		return ""
	}
	var specs []string
	if option.defaultValue != nil {
		specs = append(specs, "[default: "+*option.defaultValue+"]")
	}
	if len(option.possibleValues) > 0 {
		specs = append(specs, "[possible values: "+strings.Join(option.possibleValues, ", ")+"]")
	}
	return strings.Join(specs, " ")
}

// clapTermWidth is clap's help width without the `wrap_help` feature.
const clapTermWidth = 100

// forceNextLineHelp is clap's "force_next_line": when the names take more
// than 40% of the width, help that would not fit beside them moves under
// every name instead.
func forceNextLineHelp(longest, helpWidth int) bool {
	taken := longest + 4
	return clapTermWidth >= taken &&
		float32(taken)/float32(clapTermWidth) > 0.40 &&
		helpWidth > clapTermWidth-taken
}

// displayWidth is clap's display_width without the `unicode` feature: one
// column per character.
func displayWidth(text string) int { return utf8.RuneCountInString(text) }

// didYouMean is clap's suggestions::did_you_mean: the candidates whose Jaro
// similarity to value exceeds 0.7, least similar first.
func didYouMean(value string, candidates []string) []string {
	type scored struct {
		confidence float64
		candidate  string
	}
	var matches []scored
	for _, candidate := range candidates {
		if confidence := jaro(value, candidate); confidence > 0.7 {
			matches = append(matches, scored{confidence, candidate})
		}
	}
	slices.SortStableFunc(matches, func(a, b scored) int { return cmp.Compare(a.confidence, b.confidence) })
	var names []string
	for _, match := range matches {
		names = append(names, match.candidate)
	}
	return names
}

// jaro is strsim::jaro over characters.
func jaro(a, b string) float64 {
	left, right := []rune(a), []rune(b)
	if len(left) == 0 && len(right) == 0 {
		return 1
	}
	if len(left) == 0 || len(right) == 0 {
		return 0
	}
	searchRange := max(len(left), len(right))/2 - 1
	if searchRange < 0 {
		searchRange = 0
	}
	leftFlags := make([]bool, len(left))
	rightFlags := make([]bool, len(right))
	matches := 0
	for i, c := range left {
		low := 0
		if i > searchRange {
			low = i - searchRange
		}
		high := min(len(right), i+searchRange+1)
		for j := low; j < high; j++ {
			if c == right[j] && !rightFlags[j] {
				leftFlags[i], rightFlags[j] = true, true
				matches++
				break
			}
		}
	}
	if matches == 0 {
		return 0
	}
	transpositions := 0
	j := 0
	for i, c := range left {
		if !leftFlags[i] {
			continue
		}
		for !rightFlags[j] {
			j++
		}
		if c != right[j] {
			transpositions++
		}
		j++
	}
	transpositions /= 2
	m := float64(matches)
	return (m/float64(len(left)) + m/float64(len(right)) + (m-float64(transpositions))/m) / 3
}
