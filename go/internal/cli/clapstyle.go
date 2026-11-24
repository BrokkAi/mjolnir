// The command-line surface in the layout the Rust mj prints, so the Go
// build's help, usage and usage errors read the same. cobra finds the
// command and pflag reads the options; what clap adds on top (positional
// arguments, display order, required and conflicting arguments, groups,
// value parsers, and the help layout of clap_builder's help_template.rs) is
// here, declared beside each command by the bind methods.

package cli

import (
	"cmp"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"unicode/utf8"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// UsageError is a command-line mistake. It prints in clap's layout and exits 2.
type UsageError struct {
	Message string
	Command *cobra.Command
	// noUsage leaves out the usage line, as clap does for a value it could
	// not parse.
	noUsage bool
	// usage is the usage line clap prints for this error when it is not the
	// command's help usage: the usage of the arguments given.
	usage string
}

func (e *UsageError) Error() string { return e.Message }

// helpRequested is a command clap answers with its help on standard error
// and status 2: one whose required subcommand was not given.
type helpRequested struct{ command *cobra.Command }

func (e *helpRequested) Error() string {
	return fmt.Sprintf("'%s' requires a subcommand but one was not provided", e.command.CommandPath())
}

// clapTermWidth is the width clap lays help out for: the Rust build has no
// `wrap_help`, so clap never asks the terminal and assumes 100 columns.
const clapTermWidth = 100

// clapLastOrder is the display order of `--help` and `--version`, which
// clap adds without one and so sorts after every declared argument.
const clapLastOrder = 1 << 30

// clapArg is one argument as clap declares it.
type clapArg struct {
	// id is clap's id for the argument, the field name.
	id    string
	long  string
	short string
	// valueName is the placeholder of an option that takes a value, or of a
	// positional; empty for a flag.
	valueName string
	help      string
	env       string
	order     int
	// index is a positional's 1-based position; 0 for an option.
	index        int
	required     bool
	multiple     bool
	hidden       bool
	global       bool
	possible     []string
	defaultValue *string
	conflicts    []string
	requires     []string
	// requiredUnlessPresent names arguments any one of which lifts the
	// requirement.
	requiredUnlessPresent []string
	// requiredIfEq is (argument, value) pairs that make this one required.
	requiredIfEq [][2]string
	// parse checks and stores one value; its error follows clap's
	// "invalid value 'x' for '<arg>': " prefix.
	parse func(raw string) error
	// setFlag stores a flag's presence.
	setFlag func()
	// values is what the command line gave, in order.
	values []string
	// fromDefault is set when values holds the default rather than a value
	// the command line gave.
	fromDefault bool
	owner       *clapCommand
	// matchedAt orders the arguments the command line gave as clap's
	// matcher does, by where each first appeared: twice the number of
	// positional arguments before it (positional i takes 2i+1), then the
	// order the parser met it.
	matchedAt [2]int64
}

// clapSequence numbers the arguments as the parser meets them.
var clapSequence atomic.Int64

func (a *clapArg) positional() bool { return a.index > 0 }
func (a *clapArg) takesValue() bool { return a.valueName != "" }

// present is whether the command line gave the argument.
func (a *clapArg) present() bool { return len(a.values) > 0 && !a.fromDefault }

// value is how clap names a positional's or option's values: `<NAME>`,
// `[NAME]` for an optional positional, with `...` when it takes several.
func (a *clapArg) value(required bool) string {
	rendered := "<" + a.valueName + ">"
	if a.positional() && !required {
		rendered = "[" + a.valueName + "]"
	}
	if a.positional() && a.multiple {
		rendered += "..."
	}
	return rendered
}

// display is clap's Display of an argument: `--session <SESSION>`,
// `--json`, `[TEXT]`. required chooses a positional's brackets.
func (a *clapArg) display(required bool) string {
	switch {
	case a.positional():
		return a.value(required)
	case a.long != "" && a.takesValue():
		return "--" + a.long + " " + a.value(true)
	case a.long != "":
		return "--" + a.long
	case a.takesValue():
		return "-" + a.short + " " + a.value(true)
	}
	return "-" + a.short
}

// spec is the argument as the help lists it, before its help text.
func (a *clapArg) spec() string {
	if a.positional() {
		return a.value(a.required)
	}
	var b strings.Builder
	switch {
	case a.short != "" && a.long != "":
		b.WriteString("-" + a.short + ", --" + a.long)
	case a.long != "":
		b.WriteString("    --" + a.long)
	default:
		b.WriteString("-" + a.short)
	}
	if a.takesValue() {
		b.WriteString(" " + a.value(true))
	}
	return b.String()
}

// sortKey is clap's option_sort_key, and positional_sort_key for a
// positional.
func (a *clapArg) sortKey() (int, string) {
	if a.positional() {
		return a.index, ""
	}
	switch {
	case a.short != "":
		suffix := "0"
		if a.short != strings.ToLower(a.short) {
			suffix = "1"
		}
		return a.order, strings.ToLower(a.short) + suffix
	case a.long != "":
		return a.order, a.long
	}
	return a.order, "{" + a.id
}

// specVals is what clap writes after the help text: the environment
// variable and its value, the default, and the possible values.
func (a *clapArg) specVals() string {
	var vals []string
	if a.env != "" {
		vals = append(vals, "[env: "+a.env+"="+os.Getenv(a.env)+"]")
	}
	if a.takesValue() && a.defaultValue != nil {
		vals = append(vals, "[default: "+*a.defaultValue+"]")
	}
	if len(a.possible) > 0 {
		vals = append(vals, "[possible values: "+strings.Join(a.possible, ", ")+"]")
	}
	return strings.Join(vals, " ")
}

// clapGroup is a clap ArgGroup.
type clapGroup struct {
	id       string
	args     []string
	required bool
	multiple bool
}

// clapCommand is what clap knows about one command that cobra does not.
type clapCommand struct {
	cmd *cobra.Command
	// nextOrder is clap's current display order: every option and every
	// subcommand takes the next one, so a subcommand's own options sort
	// after the subcommands it declared first.
	nextOrder int
	args      []*clapArg
	groups    []clapGroup
	// subcommandRequired makes a missing subcommand print the help.
	subcommandRequired bool
}

var clapCommands sync.Map

// clapOf is the clap metadata of cmd, created on first use.
func clapOf(cmd *cobra.Command) *clapCommand {
	if existing, ok := clapCommands.Load(cmd); ok {
		return existing.(*clapCommand)
	}
	created, _ := clapCommands.LoadOrStore(cmd, &clapCommand{cmd: cmd})
	return created.(*clapCommand)
}

// subcommand adds child as clap adds a subcommand: it takes the parent's
// next display order.
func (c *clapCommand) subcommand(child *cobra.Command) {
	c.nextOrder++
	c.cmd.AddCommand(child)
}

// find is the argument with id, declared here or inherited.
func (c *clapCommand) find(id string) *clapArg {
	for _, arg := range c.allArgs() {
		if arg.id == id {
			return arg
		}
	}
	return nil
}

// allArgs is the command's own arguments and the global ones of its
// ancestors, the ones a closer command declares first.
func (c *clapCommand) allArgs() []*clapArg {
	args := slices.Clone(c.args)
	for parent := c.cmd.Parent(); parent != nil; parent = parent.Parent() {
		for _, arg := range clapOf(parent).args {
			if arg.global {
				args = append(args, arg)
			}
		}
	}
	return args
}

// argOption configures one argument.
type argOption func(*clapArg)

func withShort(short string) argOption    { return func(a *clapArg) { a.short = short } }
func withLong(long string) argOption      { return func(a *clapArg) { a.long = long } }
func multiple() argOption                 { return func(a *clapArg) { a.multiple = true } }
func withValueName(name string) argOption { return func(a *clapArg) { a.valueName = name } }
func withEnv(name string) argOption       { return func(a *clapArg) { a.env = name } }
func withID(id string) argOption          { return func(a *clapArg) { a.id = id } }
func required() argOption                 { return func(a *clapArg) { a.required = true } }
func hidden() argOption                   { return func(a *clapArg) { a.hidden = true } }
func global() argOption                   { return func(a *clapArg) { a.global = true } }
func possibleValues(values ...string) argOption {
	return func(a *clapArg) { a.possible = values }
}
func conflictsWith(ids ...string) argOption {
	return func(a *clapArg) { a.conflicts = append(a.conflicts, ids...) }
}
func requires(ids ...string) argOption {
	return func(a *clapArg) { a.requires = append(a.requires, ids...) }
}
func requiredUnlessPresent(ids ...string) argOption {
	return func(a *clapArg) { a.requiredUnlessPresent = append(a.requiredUnlessPresent, ids...) }
}
func requiredIfEq(id, value string) argOption {
	return func(a *clapArg) { a.requiredIfEq = append(a.requiredIfEq, [2]string{id, value}) }
}
func defaultValue(value string) argOption {
	return func(a *clapArg) { a.defaultValue = &value }
}

// longName is clap's default long name for a field: its id in kebab case.
func longName(id string) string { return strings.ReplaceAll(id, "_", "-") }

// valueNameOf is clap's default placeholder for a field: its id upper-cased.
func valueNameOf(id string) string { return strings.ToUpper(id) }

func (c *clapCommand) declare(arg *clapArg, options []argOption) *clapArg {
	arg.owner = c
	for _, option := range options {
		option(arg)
	}
	if !arg.positional() {
		arg.order = c.nextOrder
		c.nextOrder++
	}
	c.args = append(c.args, arg)
	return arg
}

// clapValue is the pflag value of an option: it keeps the raw text so the
// checks clap makes run once cobra has parsed.
type clapValue struct{ arg *clapArg }

func (v clapValue) String() string {
	if len(v.arg.values) == 0 {
		return ""
	}
	return v.arg.values[len(v.arg.values)-1]
}

func (v clapValue) Set(raw string) error {
	arg := v.arg
	if !arg.present() {
		// The positional arguments pflag has collected so far place this
		// option among them, as clap's matcher orders what it was given.
		arg.matchedAt = [2]int64{2 * int64(len(arg.owner.cmd.Flags().Args())), clapSequence.Add(1)}
	}
	if !arg.takesValue() {
		if raw != "true" {
			return &UsageError{Message: fmt.Sprintf("unexpected value '%s' for '%s' found; no more were expected", raw, arg.display(true)), Command: arg.owner.cmd, noUsage: true}
		}
		arg.values = append(arg.values, raw)
		return nil
	}
	if !arg.multiple && arg.present() {
		return &UsageError{Message: fmt.Sprintf("the argument '%s' cannot be used multiple times", arg.display(true)), Command: arg.owner.cmd}
	}
	if arg.fromDefault {
		arg.values, arg.fromDefault = nil, false
	}
	arg.values = append(arg.values, raw)
	return nil
}

func (v clapValue) Type() string {
	if v.arg.takesValue() {
		return "string"
	}
	return "bool"
}

// option declares an option that takes a value. parse checks and stores
// each value given.
func (c *clapCommand) option(id string, help string, parse func(string) error, options ...argOption) *clapArg {
	arg := c.declare(&clapArg{id: id, long: longName(id), valueName: valueNameOf(id), help: help, parse: parse}, options)
	c.register(arg)
	return arg
}

// flag declares an option that takes no value.
func (c *clapCommand) flag(target *bool, id, help string, options ...argOption) *clapArg {
	arg := c.declare(&clapArg{id: id, long: longName(id), help: help, setFlag: func() { *target = true }}, options)
	c.register(arg)
	return arg
}

func (c *clapCommand) register(arg *clapArg) {
	flags := c.cmd.Flags()
	if arg.global {
		flags = c.cmd.PersistentFlags()
	}
	flag := flags.VarPF(clapValue{arg}, arg.long, arg.short, arg.help)
	flag.Hidden = arg.hidden
	if !arg.takesValue() {
		flag.NoOptDefVal = "true"
	}
}

// positional declares the next positional argument.
func (c *clapCommand) positional(id, help string, parse func(string) error, options ...argOption) *clapArg {
	index := 1
	for _, arg := range c.args {
		if arg.positional() {
			index++
		}
	}
	return c.declare(&clapArg{id: id, index: index, valueName: valueNameOf(id), help: help, parse: parse}, options)
}

// group declares a clap ArgGroup.
func (c *clapCommand) group(id string, required, multiple bool, args ...string) {
	c.groups = append(c.groups, clapGroup{id: id, args: args, required: required, multiple: multiple})
}

// Value parsers for the field types the commands use.

func storeString(target *string) func(string) error {
	return func(raw string) error { *target = raw; return nil }
}

func storeOptional(target **string) func(string) error {
	return func(raw string) error { value := raw; *target = &value; return nil }
}

func appendString(target *[]string) func(string) error {
	return func(raw string) error { *target = append(*target, raw); return nil }
}

func storeU64(target **uint64, minimum uint64) func(string) error {
	return func(raw string) error {
		value, err := parseRustUint(raw, 64)
		if err != nil {
			return err
		}
		if value < minimum {
			return fmt.Errorf("%d is not in %d..%d", value, minimum, uint64(1<<64-1))
		}
		*target = &value
		return nil
	}
}

func storeUsize(target **int) func(string) error {
	return func(raw string) error {
		value, err := parseRustUint(raw, strconv.IntSize-1)
		if err != nil {
			return err
		}
		converted := int(value)
		*target = &converted
		return nil
	}
}

// parseRustUint parses an unsigned integer with the errors Rust's
// `str::parse` reports.
func parseRustUint(raw string, bits int) (uint64, error) {
	if raw == "" {
		return 0, errors.New("cannot parse integer from empty string")
	}
	digits := strings.TrimPrefix(raw, "+")
	if digits == "" {
		return 0, errors.New("invalid digit found in string")
	}
	for _, r := range digits {
		if r < '0' || r > '9' {
			return 0, errors.New("invalid digit found in string")
		}
	}
	value, err := strconv.ParseUint(digits, 10, bits)
	if err != nil {
		return 0, errors.New("number too large to fit in target type")
	}
	return value, nil
}

// finish assigns the positional arguments, parses every value and makes
// clap's checks. It runs once cobra has parsed the options.
func (c *clapCommand) finish(args []string) error {
	var positionals []*clapArg
	for _, arg := range c.args {
		if arg.positional() {
			positionals = append(positionals, arg)
		}
	}
	slices.SortFunc(positionals, func(a, b *clapArg) int { return cmp.Compare(a.index, b.index) })
	given := len(args)
	for _, arg := range positionals {
		if len(args) == 0 {
			break
		}
		arg.matchedAt = [2]int64{2*int64(given-len(args)) + 1, clapSequence.Add(1)}
		if arg.multiple {
			arg.values, args = append(arg.values, args...), nil
			continue
		}
		arg.values, args = append(arg.values, args[0]), args[1:]
	}
	if len(args) > 0 {
		return &UsageError{Message: fmt.Sprintf("unexpected argument '%s' found", args[0]), Command: c.cmd}
	}
	// An ancestor's own options can only come before this command's name,
	// and cobra has already read them into the ancestor; they are parsed
	// here with the rest.
	parsed := slices.Clone(c.args)
	for parent := c.cmd.Parent(); parent != nil; parent = parent.Parent() {
		parsed = append(parsed, clapOf(parent).args...)
	}
	for _, arg := range parsed {
		if !arg.present() && arg.defaultValue != nil {
			arg.values, arg.fromDefault = []string{*arg.defaultValue}, true
		}
		for _, raw := range arg.values {
			if err := c.parseValue(arg, raw); err != nil {
				return err
			}
		}
	}
	all := c.allArgs()
	matched := c.matched(all)
	if err := c.checkConflicts(all, matched); err != nil {
		return err
	}
	return c.checkRequired(all, matched)
}

// matched is the arguments the command line gave, in clap's matcher order.
func (c *clapCommand) matched(all []*clapArg) []*clapArg {
	var matched []*clapArg
	for _, arg := range all {
		if arg.present() {
			matched = append(matched, arg)
		}
	}
	slices.SortStableFunc(matched, func(a, b *clapArg) int {
		if order := cmp.Compare(a.matchedAt[0], b.matchedAt[0]); order != 0 {
			return order
		}
		return cmp.Compare(a.matchedAt[1], b.matchedAt[1])
	})
	return matched
}

// visibleIDs is the ids of the arguments that are not hidden: what clap
// names as "used" in a usage line.
func visibleIDs(args []*clapArg) []string {
	var ids []string
	for _, arg := range args {
		if !arg.hidden {
			ids = append(ids, arg.id)
		}
	}
	return ids
}

func (c *clapCommand) parseValue(arg *clapArg, raw string) error {
	if arg.setFlag != nil {
		arg.setFlag()
		return nil
	}
	if len(arg.possible) > 0 && !slices.Contains(arg.possible, raw) {
		return &UsageError{
			Message: fmt.Sprintf("invalid value '%s' for '%s'\n  [possible values: %s]", raw, arg.display(true), strings.Join(arg.possible, ", ")),
			Command: c.cmd, noUsage: true,
		}
	}
	if arg.parse == nil {
		return nil
	}
	if err := arg.parse(raw); err != nil {
		return &UsageError{Message: fmt.Sprintf("invalid value '%s' for '%s': %s", raw, arg.display(true), err), Command: c.cmd, noUsage: true}
	}
	return nil
}

func (c *clapCommand) argByID(all []*clapArg, id string) *clapArg {
	for _, arg := range all {
		if arg.id == id {
			return arg
		}
	}
	return nil
}

func (c *clapCommand) groupByID(id string) (clapGroup, bool) {
	for _, group := range c.groups {
		if group.id == id {
			return group, true
		}
	}
	return clapGroup{}, false
}

// appendUnique appends value unless it is there already, as clap's FlatSet
// does.
func appendUnique(values []string, value string) []string {
	if slices.Contains(values, value) {
		return values
	}
	return append(values, value)
}

// unrollRequires is every argument id requires, directly or through what it
// requires (clap's unroll_arg_requires).
func (c *clapCommand) unrollRequires(all []*clapArg, id string) []string {
	var required []string
	processed := map[string]bool{}
	pending := []string{id}
	for len(pending) > 0 {
		current := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if processed[current] {
			continue
		}
		processed[current] = true
		arg := c.argByID(all, current)
		if arg == nil {
			continue
		}
		for _, requirement := range arg.requires {
			if other := c.argByID(all, requirement); other != nil && len(other.requires) > 0 {
				pending = append(pending, requirement)
			}
			required = append(required, requirement)
		}
	}
	return required
}

// requiredGraph is clap's required_graph: the required arguments, then the
// required groups.
func (c *clapCommand) requiredGraph() []string {
	var required []string
	for _, arg := range c.args {
		if arg.required {
			required = append(required, arg.id)
		}
	}
	for _, group := range c.groups {
		if group.required {
			required = append(required, group.id)
		}
	}
	return required
}

// smartUsage is clap's usage of the arguments given (write_smart_usage):
// the required options and groups, the ones used, and every positional.
func (c *clapCommand) smartUsage(all []*clapArg, required, used []string) string {
	var ids []string
	for _, id := range required {
		ids = append(ids, c.unrollRequires(all, id)...)
		ids = append(ids, id)
	}
	ids = append(ids, used...)
	var groups []string
	members := map[string]bool{}
	for _, id := range ids {
		if group, ok := c.groupByID(id); ok {
			groups = appendUnique(groups, c.groupDisplay(group))
			for _, member := range group.args {
				members[member] = true
			}
		}
	}
	var options []string
	positionals := map[int]string{}
	for _, id := range ids {
		arg := c.argByID(all, id)
		if arg == nil || members[id] {
			continue
		}
		if arg.positional() {
			positionals[arg.index] = arg.value(true)
		} else {
			options = appendUnique(options, arg.display(true))
		}
	}
	for _, arg := range c.args {
		if arg.positional() && !arg.hidden && !members[arg.id] {
			if _, ok := positionals[arg.index]; !ok {
				positionals[arg.index] = arg.value(false)
			}
		}
	}
	line := "Usage: " + commandPath(c.cmd)
	for _, part := range append(options, groups...) {
		line += " " + part
	}
	for _, index := range slices.Sorted(maps.Keys(positionals)) {
		line += " " + positionals[index]
	}
	if c.subcommandRequired {
		line += " <COMMAND>"
	}
	return line
}

// directConflicts is what arg conflicts with: its own conflicts and the
// other members of each group it is in that takes one member.
func (c *clapCommand) directConflicts(arg *clapArg) []string {
	conflicts := slices.Clone(arg.conflicts)
	for _, group := range c.groups {
		if group.multiple || !slices.Contains(group.args, arg.id) {
			continue
		}
		for _, member := range group.args {
			if member != arg.id {
				conflicts = append(conflicts, member)
			}
		}
	}
	return conflicts
}

// checkConflicts is clap's validate_conflicts: in matcher order, the first
// argument given with one it conflicts with, either way round.
func (c *clapCommand) checkConflicts(all, matched []*clapArg) error {
	for _, arg := range matched {
		direct := c.directConflicts(arg)
		var conflicts []*clapArg
		for _, other := range matched {
			if other == arg {
				continue
			}
			if slices.Contains(direct, other.id) || slices.Contains(c.directConflicts(other), arg.id) {
				if !slices.Contains(conflicts, other) {
					conflicts = append(conflicts, other)
				}
			}
		}
		if len(conflicts) == 0 {
			continue
		}
		message := fmt.Sprintf("the argument '%s' cannot be used with", arg.display(arg.required))
		if len(conflicts) == 1 {
			message += fmt.Sprintf(" '%s'", conflicts[0].display(conflicts[0].required))
		} else {
			message += ":"
			for _, other := range conflicts {
				message += "\n  " + other.display(other.required)
			}
		}
		// The usage names what was given apart from the conflicting
		// arguments, after what those require.
		var used []string
		for _, given := range matched {
			if !given.hidden && !slices.Contains(conflicts, given) {
				used = append(used, given.id)
			}
		}
		var shown []string
		for _, id := range used {
			for _, requirement := range c.argByID(all, id).requires {
				conflicting := false
				for _, other := range conflicts {
					conflicting = conflicting || other.id == requirement
				}
				if !slices.Contains(used, requirement) && !conflicting {
					shown = append(shown, requirement)
				}
			}
		}
		shown = append(shown, used...)
		usage := usageLine(c.cmd)
		if len(shown) > 0 {
			usage = c.smartUsage(all, c.requiredGraph(), shown)
		}
		return &UsageError{Message: message, Command: c.cmd, usage: usage}
	}
	return nil
}

// groupDisplay is how clap names a group: `<--a <A>|--b <B>>`.
func (c *clapCommand) groupDisplay(group clapGroup) string {
	var names []string
	for _, id := range group.args {
		if arg := c.find(id); arg != nil {
			names = append(names, arg.display(true))
		}
	}
	return "<" + strings.Join(names, "|") + ">"
}

// checkRequired is clap's validate_required: the required arguments and
// groups, what the given arguments require, then the arguments required on
// a condition, each that was not given.
func (c *clapCommand) checkRequired(all, matched []*clapArg) error {
	present := func(id string) bool {
		arg := c.argByID(all, id)
		return arg != nil && arg.present()
	}
	required := c.requiredGraph()
	for _, arg := range matched {
		for _, requirement := range c.unrollRequires(all, arg.id) {
			required = appendUnique(required, requirement)
		}
	}
	var missing []string
	for _, id := range required {
		if group, ok := c.groupByID(id); ok {
			if !slices.ContainsFunc(group.args, present) {
				missing = append(missing, id)
			}
		} else if !present(id) {
			missing = append(missing, id)
		}
	}
	for _, arg := range c.args {
		if arg.present() {
			continue
		}
		conditional := false
		for _, condition := range arg.requiredIfEq {
			if other := c.argByID(all, condition[0]); other != nil && other.present() && slices.Contains(other.values, condition[1]) {
				conditional = true
			}
		}
		if len(arg.requiredUnlessPresent) > 0 && !slices.ContainsFunc(arg.requiredUnlessPresent, present) {
			conditional = true
		}
		if conditional {
			missing = append(missing, arg.id)
		}
	}
	if len(missing) == 0 {
		return nil
	}
	// The message lists what is missing as clap's get_required_usage_from
	// orders it: options, then groups, then positionals.
	var ids []string
	for _, id := range required {
		ids = append(ids, c.unrollRequires(all, id)...)
		ids = append(ids, id)
	}
	ids = append(ids, missing...)
	var groups []string
	members := map[string]bool{}
	for _, id := range ids {
		group, ok := c.groupByID(id)
		if !ok || slices.ContainsFunc(group.args, present) {
			continue
		}
		groups = appendUnique(groups, c.groupDisplay(group))
		for _, member := range group.args {
			members[member] = true
		}
	}
	var options []string
	positionals := map[int]string{}
	for _, id := range ids {
		arg := c.argByID(all, id)
		if arg == nil || members[id] || arg.present() {
			continue
		}
		if arg.positional() {
			positionals[arg.index] = arg.value(true)
		} else {
			options = appendUnique(options, arg.display(true))
		}
	}
	lines := append(options, groups...)
	for _, index := range slices.Sorted(maps.Keys(positionals)) {
		lines = append(lines, positionals[index])
	}
	return &UsageError{
		Message: "the following required arguments were not provided:\n  " + strings.Join(lines, "\n  "),
		Command: c.cmd,
		usage:   c.smartUsage(all, required, append(visibleIDs(matched), missing...)),
	}
}

// helpArgs are `-h, --help` and, on the root, `-V, --version`.
func helpArgs(cmd *cobra.Command) []*clapArg {
	args := []*clapArg{{id: "help", long: "help", short: "h", help: "Print help", order: clapLastOrder}}
	if cmd.Version != "" {
		args = append(args, &clapArg{id: "version", long: "version", short: "V", help: "Print version", order: clapLastOrder})
	}
	return args
}

// commandPath is how clap names a command in usage lines: "mj sessions".
func commandPath(cmd *cobra.Command) string { return cmd.CommandPath() }

func visibleSubcommands(cmd *cobra.Command) []*cobra.Command {
	var visible []*cobra.Command
	for _, sub := range cmd.Commands() {
		if !sub.Hidden && sub.Name() != "completion" {
			visible = append(visible, sub)
		}
	}
	return visible
}

// usageLine is clap's help usage: the command, `[OPTIONS]` when there is
// an optional option, the required options, groups and positionals, and the
// subcommand placeholder.
func usageLine(cmd *cobra.Command) string {
	meta := clapOf(cmd)
	all := meta.allArgs()
	inRequiredGroup := func(id string) bool {
		for _, group := range meta.groups {
			if group.required && slices.Contains(group.args, id) {
				return true
			}
		}
		return false
	}
	line := "Usage: " + commandPath(cmd)
	for _, arg := range all {
		if !arg.positional() && !arg.hidden && !arg.required && !inRequiredGroup(arg.id) {
			line += " [OPTIONS]"
			break
		}
	}
	for _, arg := range meta.args {
		if !arg.positional() && arg.required {
			line += " " + arg.display(true)
		}
	}
	for _, group := range meta.groups {
		if group.required {
			line += " " + meta.groupDisplay(group)
		}
	}
	var positionals []*clapArg
	for _, arg := range meta.args {
		if arg.positional() && !arg.hidden && !inRequiredGroup(arg.id) {
			positionals = append(positionals, arg)
		}
	}
	slices.SortFunc(positionals, func(a, b *clapArg) int { return cmp.Compare(a.index, b.index) })
	for _, arg := range positionals {
		line += " " + arg.value(arg.required)
	}
	if visibleSubcommands(cmd) != nil {
		if meta.subcommandRequired {
			line += " <COMMAND>"
		} else {
			line += " [COMMAND]"
		}
	}
	return line
}

// displayWidth is the number of columns text takes, as clap counts it.
func displayWidth(text string) int { return utf8.RuneCountInString(text) }

// writeArgs is clap's write_args: one row per argument, sorted by display
// order, with the help beside the widest spec, or on the next line when one
// help would not fit beside it.
func writeArgs(b *strings.Builder, args []*clapArg) {
	longest := 2
	for _, arg := range args {
		width := displayWidth(arg.display(arg.required))
		if !arg.positional() {
			width = displayWidth(arg.spec())
		}
		longest = max(longest, width)
	}
	slices.SortStableFunc(args, func(a, b *clapArg) int {
		aOrder, aKey := a.sortKey()
		bOrder, bKey := b.sortKey()
		if order := cmp.Compare(aOrder, bOrder); order != 0 {
			return order
		}
		return cmp.Compare(aKey, bKey)
	})
	nextLine := false
	for _, arg := range args {
		taken := longest + 4
		helpWidth := displayWidth(arg.help) + displayWidth(arg.specVals())
		if clapTermWidth >= taken && float32(taken)/float32(clapTermWidth) > 0.40 && helpWidth > clapTermWidth-taken {
			nextLine = true
		}
	}
	for i, arg := range args {
		if i != 0 {
			b.WriteString("\n")
		}
		spec := arg.spec()
		b.WriteString("  " + spec)
		spaces := longest + 4
		if nextLine {
			b.WriteString("\n          ")
			spaces = 10
		} else {
			b.WriteString(strings.Repeat(" ", longest+2-displayWidth(spec)))
		}
		help := arg.help
		if vals := arg.specVals(); vals != "" {
			if help != "" {
				help += " "
			}
			help += vals
		}
		b.WriteString(strings.ReplaceAll(help, "\n", "\n"+strings.Repeat(" ", spaces)))
	}
}

// writeHelp writes cmd's help as clap's default template lays it out. None
// of mj's commands has a long help, so `-h` and `--help` print the same.
func writeHelp(out io.Writer, cmd *cobra.Command) {
	var b strings.Builder
	if cmd.Short != "" {
		b.WriteString(cmd.Short + "\n\n")
	}
	b.WriteString(usageLine(cmd))

	var sections []string
	if subs := visibleSubcommands(cmd); subs != nil {
		var section strings.Builder
		section.WriteString("Commands:\n")
		width := len("help")
		for _, sub := range subs {
			width = max(width, displayWidth(sub.Name()))
		}
		for _, sub := range subs {
			fmt.Fprintf(&section, "  %s%s  %s\n", sub.Name(), strings.Repeat(" ", width-displayWidth(sub.Name())), sub.Short)
		}
		fmt.Fprintf(&section, "  %-*s  %s", width, "help", "Print this message or the help of the given subcommand(s)")
		sections = append(sections, section.String())
	}
	var positionals, options []*clapArg
	for _, arg := range clapOf(cmd).allArgs() {
		switch {
		case arg.hidden:
		case arg.positional():
			positionals = append(positionals, arg)
		default:
			options = append(options, arg)
		}
	}
	if len(positionals) > 0 {
		var section strings.Builder
		section.WriteString("Arguments:\n")
		writeArgs(&section, positionals)
		sections = append(sections, section.String())
	}
	var section strings.Builder
	section.WriteString("Options:\n")
	writeArgs(&section, append(options, helpArgs(cmd)...))
	sections = append(sections, section.String())

	b.WriteString("\n\n" + strings.Join(sections, "\n\n"))
	io.WriteString(out, strings.TrimRight(b.String(), " \t\n")+"\n")
}

// installClapStyle replaces cobra's help, version and error output.
func installClapStyle(root *cobra.Command) {
	root.SetHelpFunc(func(cmd *cobra.Command, _ []string) {
		writeHelp(cmd.OutOrStdout(), cmd)
	})
	root.SetVersionTemplate("{{.Name}} {{.Version}}\n")
	root.SetFlagErrorFunc(clapFlagError)
	root.SilenceErrors = true
	root.SilenceUsage = true
	root.CompletionOptions.DisableDefaultCmd = true
	root.TraverseChildren = true
}

// clapFlagError says what pflag reported in clap's words.
func clapFlagError(cmd *cobra.Command, err error) error {
	var usage *UsageError
	if errors.As(err, &usage) {
		return usage
	}
	var notExist *pflag.NotExistError
	if errors.As(err, &notExist) {
		name := "--" + notExist.GetSpecifiedName()
		if shorthands := notExist.GetSpecifiedShortnames(); shorthands != "" {
			name = "-" + notExist.GetSpecifiedName()[:1]
		}
		meta := clapOf(cmd)
		all := meta.allArgs()
		usage := usageLine(cmd)
		if used := visibleIDs(meta.matched(all)); len(used) > 0 {
			usage = meta.smartUsage(all, meta.requiredGraph(), used)
		}
		return &UsageError{Message: fmt.Sprintf("unexpected argument '%s' found", name), Command: cmd, usage: usage}
	}
	var valueRequired *pflag.ValueRequiredError
	if errors.As(err, &valueRequired) {
		name := valueRequired.GetSpecifiedName()
		for _, arg := range clapOf(cmd).allArgs() {
			if arg.long == name || arg.short == name {
				return &UsageError{Message: fmt.Sprintf("a value is required for '%s' but none was supplied", arg.display(true)), Command: cmd, noUsage: true}
			}
		}
	}
	return &UsageError{Message: err.Error(), Command: cmd}
}

// unrecognizedSubcommand is the Args check of a command whose positional
// arguments can only be subcommands.
func unrecognizedSubcommand(cmd *cobra.Command, args []string) error {
	if len(args) > 0 {
		return &UsageError{Message: fmt.Sprintf("unrecognized subcommand '%s'", args[0]), Command: cmd}
	}
	return nil
}

// addHelpSubcommand adds clap's `help` subcommand to a command that has
// subcommands: `mj daemon help stop` prints the help of `mj daemon stop`.
// The help listing writes its row itself.
func addHelpSubcommand(parent *cobra.Command) {
	parent.AddCommand(&cobra.Command{
		Use:    "help",
		Hidden: true,
		Args:   cobra.ArbitraryArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			target := parent
			for _, name := range args {
				var found *cobra.Command
				for _, sub := range target.Commands() {
					if sub.Name() == name && sub.Name() != "help" {
						found = sub
					}
				}
				if found == nil {
					return &UsageError{Message: fmt.Sprintf("unrecognized subcommand '%s'", name), Command: target}
				}
				target = found
			}
			writeHelp(cmd.OutOrStdout(), target)
			return nil
		},
	})
}

// reportError prints err the way the Rust mj does and returns the exit code.
// A usage error prints the command's help usage; clap prints the usage of
// the arguments given, which reads the same for the commands mj has.
func reportError(stderr io.Writer, err error) int {
	var usage *UsageError
	if errors.As(err, &usage) {
		switch {
		case usage.noUsage || usage.Command == nil:
			fmt.Fprintf(stderr, "error: %s\n\nFor more information, try '--help'.\n", usage.Message)
		case usage.usage != "":
			fmt.Fprintf(stderr, "error: %s\n\n%s\n\nFor more information, try '--help'.\n", usage.Message, usage.usage)
		default:
			fmt.Fprintf(stderr, "error: %s\n\n%s\n\nFor more information, try '--help'.\n", usage.Message, usageLine(usage.Command))
		}
		return 2
	}
	var help *helpRequested
	if errors.As(err, &help) {
		writeHelp(stderr, help.command)
		return 2
	}
	var exit *exitStatus
	if errors.As(err, &exit) {
		return exit.code
	}
	fmt.Fprintf(stderr, "Error: %s\n", err)
	return 1
}

// exitStatus ends the process with code after the command has said what it
// had to (Rust's std::process::exit).
type exitStatus struct{ code int }

func (e *exitStatus) Error() string { return fmt.Sprintf("exit status %d", e.code) }
