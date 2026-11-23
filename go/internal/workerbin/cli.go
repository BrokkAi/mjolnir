// Part of the port of mj-worker-bin/src/main.rs: the clap-derived command
// line (`Cli`, `Command`, `WorkerArgs`, `AuthorizedKeysArgs` and their
// subcommands), parsed with cobra.
//
// Rust derives the parser with clap 4. Cobra parses here; help, version and
// usage errors are rendered in clap's layout and words and leave with
// clap's statuses: 0 for help and version on standard output, 2 for a usage
// error on standard error. The controller's control scripts probe a worker
// with `worker <subcommand> --help` and take status 2 for a worker too old
// to know the subcommand, so those statuses are part of the worker's
// contract (testdata/tests/golden/worker_control_contract.txt in
// go/cmd/mj-worker).
//
// clap's derive names each option after its field in kebab case
// (`expected_sha256` is `--expected-sha256`, `_root` is `--root`); an
// option of a plain type is required, an Option or bool is not, and a Vec
// is repeatable. The hidden global options may follow the subcommand, which
// is where the login re-exec appends them.

package workerbin

import (
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strings"

	"github.com/spf13/cobra"
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
// a usage error.
func (e *clapError) exitCode() int {
	if e.kind == clapErrorUsage {
		return 2
	}
	return 0
}

// print writes the text where clap writes it, help and version to standard
// output and usage errors to standard error, and returns the status (clap's
// Error::exit, without the exit).
func (e *clapError) print() int {
	var out io.Writer = os.Stdout
	if e.kind == clapErrorUsage {
		out = os.Stderr
	}
	_, _ = io.WriteString(out, e.text)
	return e.exitCode()
}

// parseCli is clap's Cli::try_parse_from: args[0] is the program name.
// Help, version and usage errors come back as *clapError.
func parseCli(args []string) (*cli, error) {
	panic("unported: mj-worker-bin/src/main.rs Cli::try_parse_from")
}

// newCliCommand builds the command tree: the root with its version and its
// hidden global `--login-environment-ready` and `--worker-root-lock-fd`, the
// hidden `worker` command, and every worker subcommand with clap's names,
// hidden flags, defaults, value lists, conflicts and requirements. Running
// a leaf stores its parsed command in parsed.
func newCliCommand(parsed *cli) *cobra.Command {
	panic("unported: mj-worker-bin/src/main.rs Cli (clap derive)")
}

// clapHelp is cmd's help in clap's layout: about, `Usage:` line,
// `Commands:` (visible subcommands and `help`), `Options:` with each
// option's value name, default and possible values, then `-h, --help` and,
// on the root, `-V, --version`. Hidden commands and options are left out.
func clapHelp(cmd *cobra.Command) string {
	panic("unported: mj-worker-bin/src/main.rs Cli (clap help)")
}

// clapUsageError is clap's usage error for cmd: `error: <message>`, a blank
// line, the usage line, and the tip "For more information, try '--help'.".
func clapUsageError(cmd *cobra.Command, message string) *clapError {
	panic("unported: mj-worker-bin/src/main.rs Cli (clap usage error)")
}

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
