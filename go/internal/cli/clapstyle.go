// Help and usage-error rendering in the layout the Rust mj prints, so the Go
// build's command-line surface reads the same. Parsing itself is cobra's.

package cli

import (
	"cmp"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// Flag annotations carrying what clap shows and cobra does not track.
const (
	annotationOrder     = "mj_order"
	annotationEnv       = "mj_env"
	annotationValueName = "mj_value_name"
	annotationLongHelp  = "mj_long_help"
)

// UsageError is a command-line mistake. It prints in clap's layout and exits 2.
type UsageError struct {
	Message string
	Command *cobra.Command
}

func (e *UsageError) Error() string { return e.Message }

// flagSpec declares one option. Order is the position clap gives it within
// the command that declares it.
type flagSpec struct {
	Name, Short, ValueName, Env, Help, LongHelp string
	Order                                       int
}

func annotate(flags *pflag.FlagSet, spec flagSpec) {
	flag := flags.Lookup(spec.Name)
	set := func(key, value string) {
		if value != "" {
			flags.SetAnnotation(spec.Name, key, []string{value})
		}
	}
	set(annotationOrder, strconv.Itoa(spec.Order))
	set(annotationEnv, spec.Env)
	set(annotationValueName, spec.ValueName)
	set(annotationLongHelp, spec.LongHelp)
	flag.Usage = spec.Help
}

func boolFlag(flags *pflag.FlagSet, target *bool, spec flagSpec) {
	flags.BoolVarP(target, spec.Name, spec.Short, false, spec.Help)
	annotate(flags, spec)
}

func stringFlag(flags *pflag.FlagSet, target *string, spec flagSpec) {
	flags.StringVarP(target, spec.Name, spec.Short, "", spec.Help)
	annotate(flags, spec)
}

func annotation(flag *pflag.Flag, key string) string {
	if values := flag.Annotations[key]; len(values) > 0 {
		return values[0]
	}
	return ""
}

// commandPath is how clap names a command in usage lines: "mj sessions".
func commandPath(cmd *cobra.Command) string { return cmd.CommandPath() }

func usageLine(cmd *cobra.Command) string {
	line := "Usage: " + commandPath(cmd) + " [OPTIONS]"
	if positional := cmd.Annotations["mj_positional"]; positional != "" {
		line += " " + positional
	}
	if visibleSubcommands(cmd) != nil {
		if cmd.Runnable() {
			line += " [COMMAND]"
		} else {
			line += " <COMMAND>"
		}
	}
	return line
}

func visibleSubcommands(cmd *cobra.Command) []*cobra.Command {
	var visible []*cobra.Command
	for _, sub := range cmd.Commands() {
		if !sub.Hidden && sub.Name() != "completion" {
			visible = append(visible, sub)
		}
	}
	return visible
}

type optionRow struct {
	spec, help string
	order      int
	global     bool
}

func optionRows(cmd *cobra.Command, long bool) []optionRow {
	var rows []optionRow
	add := func(flag *pflag.Flag, global bool) {
		if flag.Hidden || flag.Name == "help" || flag.Name == "version" {
			return
		}
		spec := "    "
		if flag.Shorthand != "" {
			spec = "-" + flag.Shorthand + ", "
		}
		spec += "--" + flag.Name
		if valueName := annotation(flag, annotationValueName); valueName != "" {
			spec += " <" + valueName + ">"
		}
		help := flag.Usage
		if long {
			if text := annotation(flag, annotationLongHelp); text != "" {
				help = text
			}
		}
		if env := annotation(flag, annotationEnv); env != "" {
			help += " [env: " + env + "=" + os.Getenv(env) + "]"
		}
		order, _ := strconv.Atoi(annotation(flag, annotationOrder))
		rows = append(rows, optionRow{spec: spec, help: help, order: order, global: global})
	}
	cmd.InheritedFlags().VisitAll(func(flag *pflag.Flag) { add(flag, true) })
	cmd.LocalNonPersistentFlags().VisitAll(func(flag *pflag.Flag) { add(flag, false) })
	cmd.PersistentFlags().VisitAll(func(flag *pflag.Flag) { add(flag, cmd.HasParent()) })
	// clap sorts by declaration index; an index inherited from the parent ties
	// with the child's own and sorts first.
	slices.SortStableFunc(rows, func(a, b optionRow) int {
		if c := cmp.Compare(a.order, b.order); c != 0 {
			return c
		}
		switch {
		case a.global && !b.global:
			return -1
		case !a.global && b.global:
			return 1
		}
		return 0
	})
	rows = append(rows, optionRow{spec: "-h, --help", help: "Print help"})
	if cmd.Version != "" {
		rows = append(rows, optionRow{spec: "-V, --version", help: "Print version"})
	}
	return rows
}

func writeHelp(out io.Writer, cmd *cobra.Command, long bool) {
	var b strings.Builder
	about := cmd.Short
	if long && cmd.Long != "" {
		about = cmd.Long
	}
	if about != "" {
		b.WriteString(about + "\n\n")
	}
	b.WriteString(usageLine(cmd) + "\n")

	if subs := visibleSubcommands(cmd); subs != nil {
		width := len("help")
		for _, sub := range subs {
			width = max(width, len(sub.Name()))
		}
		b.WriteString("\nCommands:\n")
		for _, sub := range subs {
			fmt.Fprintf(&b, "  %-*s  %s\n", width, sub.Name(), sub.Short)
		}
		fmt.Fprintf(&b, "  %-*s  %s\n", width, "help", "Print this message or the help of the given subcommand(s)")
	}

	rows := optionRows(cmd, long)
	width := 0
	for _, row := range rows {
		width = max(width, len(row.spec))
	}
	b.WriteString("\nOptions:\n")
	for _, row := range rows {
		fmt.Fprintf(&b, "  %-*s  %s\n", width, row.spec, row.help)
	}
	io.WriteString(out, b.String())
}

// installClapStyle replaces cobra's help, version and error output.
// clap shows long help for --help and short help for -h; cobra treats both
// as one flag, so the arguments decide.
func installClapStyle(root *cobra.Command, args []string) {
	root.SetHelpFunc(func(cmd *cobra.Command, _ []string) {
		writeHelp(cmd.OutOrStdout(), cmd, slices.Contains(args, "--help"))
	})
	root.SetVersionTemplate("{{.Name}} {{.Version}}\n")
	root.SetFlagErrorFunc(func(cmd *cobra.Command, err error) error {
		message := err.Error()
		if name, ok := strings.CutPrefix(message, "unknown flag: "); ok {
			message = fmt.Sprintf("unexpected argument '%s' found", name)
		} else if name, ok := strings.CutPrefix(message, "unknown shorthand flag: "); ok {
			message = fmt.Sprintf("unexpected argument '-%s' found", strings.SplitN(name, " ", 2)[0][1:2])
		}
		return &UsageError{Message: message, Command: cmd}
	})
	root.SilenceErrors = true
	root.SilenceUsage = true
	root.CompletionOptions.DisableDefaultCmd = true
}

// unrecognizedSubcommand is the Args check of a command whose positional
// arguments can only be subcommands.
func unrecognizedSubcommand(cmd *cobra.Command, args []string) error {
	if len(args) > 0 {
		return &UsageError{Message: fmt.Sprintf("unrecognized subcommand '%s'", args[0]), Command: cmd}
	}
	return nil
}

// reportError prints err the way the Rust mj does and returns the exit code.
func reportError(stderr io.Writer, err error) int {
	var usage *UsageError
	if errors.As(err, &usage) {
		fmt.Fprintf(stderr, "error: %s\n\n%s\n\nFor more information, try '--help'.\n", usage.Message, usageLine(usage.Command))
		return 2
	}
	fmt.Fprintf(stderr, "Error: %s\n", err)
	return 1
}
