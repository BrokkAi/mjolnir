//go:build linux

// Part of the port of mj-microvm/src/cli.rs.
//
// Rust parses the engine's command line with clap 4's derive and lets clap
// print help, version and usage errors. This file is the part of clap that
// command line exercises, so the Go build accepts, refuses and prints
// exactly what Rust does: long and short options (`--name x`, `--name=x`,
// `-n x`, `-nx`, clustered flags), positionals, a trailing argument that
// takes hyphenated words, `--`, the `help` subcommand, and clap's help
// layout and error text, including its "a similar argument exists" tips
// (Jaro similarity above 0.7, as clap's suggestions feature computes it).
// Output carries no colour, as clap's does when it is not on a terminal,
// and help is never wrapped (clap's `wrap_help` feature is off).

package microvm

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

// microvmClapAction is what an argument does when it appears (clap's
// ArgAction).
type microvmClapAction int

const (
	microvmClapSet microvmClapAction = iota
	microvmClapAppend
	microvmClapSetTrue
	microvmClapHelp
	microvmClapVersion
)

// microvmClapArg is one argument of a command.
type microvmClapArg struct {
	// id is the Rust field name, which the matches are keyed by.
	id    string
	short byte
	long  string
	// value is the value name (`<NAME>`); empty for flags.
	value      string
	help       string
	required   bool
	multiple   bool
	positional bool
	// trailing is `trailing_var_arg = true, allow_hyphen_values = true`.
	trailing bool
	action   microvmClapAction
	// parse is the value parser; nil keeps the text.
	parse func(string) (any, error)
}

func (a *microvmClapArg) takesValue() bool {
	return a.action == microvmClapSet || a.action == microvmClapAppend
}

// stylized is clap's Arg::stylized: `--name <NAME>`, `--detach`, `<IMAGE>`,
// `[COMMAND]...`.
func (a *microvmClapArg) stylized(required bool) string {
	if a.positional {
		text := "<" + a.value + ">"
		if !required {
			text = "[" + a.value + "]"
		}
		if a.multiple {
			text += "..."
		}
		return text
	}
	text := "-" + string(a.short)
	if a.long != "" {
		text = "--" + a.long
	}
	if a.takesValue() {
		text += " <" + a.value + ">"
	}
	return text
}

// display is the argument as clap's Display writes it.
func (a *microvmClapArg) display() string {
	return a.stylized(a.required)
}

// microvmClapCommand is one command and its subcommands.
type microvmClapCommand struct {
	name        string
	about       string
	args        []*microvmClapArg
	subcommands []*microvmClapCommand
	hidden      bool
	// version is the `--version` text; only the top command has one.
	version string
	binName string
	// isHelp marks the `help` subcommand clap generates.
	isHelp bool
}

// build adds what clap adds to every command: `-h, --help`, the top
// command's `-V, --version`, and a `help` subcommand where there are
// subcommands.
func (c *microvmClapCommand) build(binName string) {
	c.binName = binName
	if !c.isHelp {
		c.args = append(c.args, &microvmClapArg{id: "help", short: 'h', long: "help", help: "Print help", action: microvmClapHelp})
		if c.version != "" {
			c.args = append(c.args, &microvmClapArg{id: "version", short: 'V', long: "version", help: "Print version", action: microvmClapVersion})
		}
	}
	if len(c.subcommands) > 0 {
		c.subcommands = append(c.subcommands, &microvmClapCommand{
			name:   "help",
			about:  "Print this message or the help of the given subcommand(s)",
			isHelp: true,
			args: []*microvmClapArg{{
				id: "subcommand", value: "COMMAND", help: "Print help for the subcommand(s)",
				positional: true, multiple: true, action: microvmClapAppend,
			}},
		})
	}
	for _, sub := range c.subcommands {
		sub.build(binName + " " + sub.name)
	}
}

func (c *microvmClapCommand) positionals() []*microvmClapArg {
	var positionals []*microvmClapArg
	for _, arg := range c.args {
		if arg.positional {
			positionals = append(positionals, arg)
		}
	}
	return positionals
}

func (c *microvmClapCommand) find(id string) *microvmClapArg {
	for _, arg := range c.args {
		if arg.id == id {
			return arg
		}
	}
	return nil
}

func (c *microvmClapCommand) findLong(long string) *microvmClapArg {
	for _, arg := range c.args {
		if arg.long != "" && arg.long == long {
			return arg
		}
	}
	return nil
}

func (c *microvmClapCommand) findShort(short rune) *microvmClapArg {
	for _, arg := range c.args {
		if arg.short != 0 && rune(arg.short) == short {
			return arg
		}
	}
	return nil
}

func (c *microvmClapCommand) findSubcommand(name string) *microvmClapCommand {
	for _, sub := range c.subcommands {
		if sub.name == name {
			return sub
		}
	}
	return nil
}

func (c *microvmClapCommand) longs() []string {
	var longs []string
	for _, arg := range c.args {
		if arg.long != "" {
			longs = append(longs, arg.long)
		}
	}
	return longs
}

// ---------------------------------------------------------------------------
// Usage and help

// needsOptionsTag is whether help usage shows `[OPTIONS]`: some option other
// than help and version that is not required.
func (c *microvmClapCommand) needsOptionsTag() bool {
	for _, arg := range c.args {
		if !arg.positional && arg.action != microvmClapHelp && arg.action != microvmClapVersion && !arg.required {
			return true
		}
	}
	return false
}

// usage is clap's usage line without its title: the help form when used is
// empty, otherwise the "smart" form that names the required arguments and
// the ones used.
func (c *microvmClapCommand) usage(used []string) string {
	parts := []string{c.binName}
	if len(used) == 0 && c.needsOptionsTag() {
		parts = append(parts, "[OPTIONS]")
	}
	positionals := c.positionals()
	slots := map[*microvmClapArg]string{}
	var options []string
	seen := map[string]bool{}
	include := func(arg *microvmClapArg) {
		text := arg.stylized(true)
		if arg.positional {
			slots[arg] = text
		} else if !seen[text] {
			seen[text] = true
			options = append(options, text)
		}
	}
	for _, arg := range c.args {
		if arg.required {
			include(arg)
		}
	}
	for _, id := range used {
		if arg := c.find(id); arg != nil {
			include(arg)
		}
	}
	parts = append(parts, options...)
	for _, positional := range positionals {
		if text, ok := slots[positional]; ok {
			parts = append(parts, text)
		} else {
			parts = append(parts, positional.stylized(false))
		}
	}
	if len(c.subcommands) > 0 {
		parts = append(parts, "<COMMAND>")
	}
	return strings.Join(parts, " ")
}

// microvmClapTermWidth is the width clap lays help out for without its
// `wrap_help` feature.
const microvmClapTermWidth = 100

// microvmClapNextLine is clap's test for putting help on the line after
// the argument: the column is wide and the text would not fit beside it.
func microvmClapNextLine(longest int, helps []string) bool {
	taken := longest + 4
	for _, help := range helps {
		if microvmClapTermWidth >= taken && float32(taken)/float32(microvmClapTermWidth) > 0.40 && utf8.RuneCountInString(help) > microvmClapTermWidth-taken {
			return true
		}
	}
	return false
}

// microvmClapRow writes one help row: the spec padded to the help column,
// or the help on the next line.
func microvmClapRow(spec string, padding int, help string, nextLine bool) string {
	if nextLine {
		return "  " + spec + "\n" + "          " + help
	}
	return "  " + spec + strings.Repeat(" ", max(padding, 0)) + help
}

func microvmClapArgRows(args []*microvmClapArg) string {
	longest := 2
	helps := make([]string, len(args))
	for i, arg := range args {
		width := utf8.RuneCountInString(arg.display())
		if arg.long != "" {
			width += 4
		}
		longest = max(longest, width)
		helps[i] = arg.help
	}
	nextLine := microvmClapNextLine(longest, helps)
	rows := make([]string, len(args))
	for i, arg := range args {
		if arg.positional {
			spec := arg.display()
			rows[i] = microvmClapRow(spec, longest+2-utf8.RuneCountInString(spec), arg.help, nextLine)
			continue
		}
		spec := ""
		switch {
		case arg.short != 0 && arg.long != "":
			spec = "-" + string(arg.short) + ", --" + arg.long
		case arg.short != 0:
			spec = "-" + string(arg.short)
		default:
			spec = "    --" + arg.long
		}
		if arg.takesValue() {
			spec += " <" + arg.value + ">"
		}
		selfWidth := utf8.RuneCountInString(arg.display()) + 4
		padding := longest + 2 - selfWidth
		if arg.long == "" {
			padding = longest + 2 + 4 - selfWidth
		}
		rows[i] = microvmClapRow(spec, padding, arg.help, nextLine)
	}
	return strings.Join(rows, "\n")
}

func (c *microvmClapCommand) subcommandRows() string {
	var visible []*microvmClapCommand
	longest := 2
	for _, sub := range c.subcommands {
		if !sub.hidden {
			visible = append(visible, sub)
			longest = max(longest, utf8.RuneCountInString(sub.name))
		}
	}
	abouts := make([]string, len(visible))
	for i, sub := range visible {
		abouts[i] = sub.about
	}
	nextLine := microvmClapNextLine(longest, abouts)
	rows := make([]string, len(visible))
	for i, sub := range visible {
		rows[i] = microvmClapRow(sub.name, longest+2-utf8.RuneCountInString(sub.name), sub.about, nextLine)
	}
	return strings.Join(rows, "\n")
}

// helpText is clap's help for the command: its about, usage, commands,
// arguments and options.
func (c *microvmClapCommand) helpText() string {
	var builder strings.Builder
	if c.about != "" {
		builder.WriteString(c.about)
		builder.WriteString("\n")
	}
	builder.WriteString("\nUsage: " + c.usage(nil))
	var sections []string
	hasVisible := false
	for _, sub := range c.subcommands {
		hasVisible = hasVisible || !sub.hidden
	}
	if hasVisible {
		sections = append(sections, "Commands:\n"+c.subcommandRows())
	}
	var positionals, options []*microvmClapArg
	for _, arg := range c.args {
		if arg.positional {
			positionals = append(positionals, arg)
		} else {
			options = append(options, arg)
		}
	}
	if len(positionals) > 0 {
		sections = append(sections, "Arguments:\n"+microvmClapArgRows(positionals))
	}
	if len(options) > 0 {
		sections = append(sections, "Options:\n"+microvmClapArgRows(options))
	}
	if len(sections) > 0 {
		builder.WriteString("\n\n" + strings.Join(sections, "\n\n"))
	}
	text := builder.String()
	// clap drops a blank first line (a command without an about) and
	// trailing whitespace, and ends with one newline.
	if first, rest, ok := strings.Cut(text, "\n"); ok && strings.TrimSpace(first) == "" {
		text = rest
	}
	return strings.TrimRightFunc(text, unicode.IsSpace) + "\n"
}

// ---------------------------------------------------------------------------
// Errors

// microvmClapErrorText is clap's rich error format.
type microvmClapErrorText struct {
	message     string
	similarKind string
	similar     []string
	tips        []string
	usage       string
}

func (c *microvmClapCommand) fail(e microvmClapErrorText) error {
	var builder strings.Builder
	builder.WriteString("error: " + e.message)
	suggested := false
	if len(e.similar) > 0 {
		builder.WriteString("\n\n  tip:")
		suggested = true
		quoted := make([]string, len(e.similar))
		for i, similar := range e.similar {
			quoted[i] = "'" + similar + "'"
		}
		if len(quoted) == 1 {
			fmt.Fprintf(&builder, " a similar %s exists: %s", e.similarKind, quoted[0])
		} else {
			fmt.Fprintf(&builder, " some similar %ss exist: %s", e.similarKind, strings.Join(quoted, ", "))
		}
	}
	if len(e.tips) > 0 {
		if !suggested {
			builder.WriteString("\n")
		}
		for _, tip := range e.tips {
			builder.WriteString("\n  tip: " + tip)
		}
	}
	if e.usage != "" {
		builder.WriteString("\n\nUsage: " + e.usage)
	}
	if c.isHelp {
		// The help subcommand has no help flag to point at.
		builder.WriteString("\n")
	} else {
		builder.WriteString("\n\nFor more information, try '--help'.\n")
	}
	return &microvmCLIError{text: builder.String(), useStderr: true}
}

func (c *microvmClapCommand) unrecognizedSubcommand(name string) error {
	return c.fail(microvmClapErrorText{message: fmt.Sprintf("unrecognized subcommand '%s'", name), usage: c.usage(nil)})
}

// microvmClapJaro is strsim's Jaro similarity over characters.
func microvmClapJaro(a, b string) float64 {
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
	for i, l := range left {
		low := 0
		if i > searchRange {
			low = i - searchRange
		}
		high := min(len(right), i+searchRange+1)
		for j := low; j < high; j++ {
			if l == right[j] && !rightFlags[j] {
				leftFlags[i], rightFlags[j] = true, true
				matches++
				break
			}
		}
	}
	if matches == 0 {
		return 0
	}
	transpositions, j := 0, 0
	for i, l := range left {
		if !leftFlags[i] {
			continue
		}
		for !rightFlags[j] {
			j++
		}
		if l != right[j] {
			transpositions++
		}
		j++
	}
	transpositions /= 2
	m := float64(matches)
	return (m/float64(len(left)) + m/float64(len(right)) + (m-float64(transpositions))/m) / 3
}

// microvmClapDidYouMean is clap's did_you_mean: the candidates more than
// 0.7 similar to value, least similar first (an equal one after those
// already found).
func microvmClapDidYouMean(value string, candidates []string) []string {
	type scored struct {
		confidence float64
		name       string
	}
	var found []scored
	for _, candidate := range candidates {
		confidence := microvmClapJaro(value, candidate)
		if confidence <= 0.7 {
			continue
		}
		position := len(found)
		for i, existing := range found {
			if existing.confidence > confidence {
				position = i
				break
			}
		}
		found = append(found, scored{})
		copy(found[position+1:], found[position:])
		found[position] = scored{confidence, candidate}
	}
	names := make([]string, len(found))
	for i, entry := range found {
		names[i] = entry.name
	}
	return names
}

func microvmClapIsLong(arg string) bool { return strings.HasPrefix(arg, "--") && arg != "--" }

func microvmClapIsShort(arg string) bool {
	return strings.HasPrefix(arg, "-") && arg != "-" && !strings.HasPrefix(arg, "--")
}

// ---------------------------------------------------------------------------
// Parsing

// microvmClapMatches is what a command's parse found.
type microvmClapMatches struct {
	values  map[string][]string
	parsed  map[string]any
	present map[string]bool
	// order is the arguments given, in the order clap recorded them.
	order []string
	sub   *microvmClapSubcommand
}

type microvmClapSubcommand struct {
	name    string
	matches *microvmClapMatches
}

func (m *microvmClapMatches) one(id string) *string {
	values, ok := m.values[id]
	if !ok || len(values) == 0 {
		return nil
	}
	value := values[len(values)-1]
	return &value
}

func (m *microvmClapMatches) many(id string) []string {
	return append([]string(nil), m.values[id]...)
}

func (m *microvmClapMatches) flag(id string) bool { return m.present[id] }

type microvmClapParser struct {
	cmd     *microvmClapCommand
	matches *microvmClapMatches
	pending *microvmClapArg
	values  []string
}

func (p *microvmClapParser) mark(arg *microvmClapArg) {
	if !p.matches.present[arg.id] {
		p.matches.present[arg.id] = true
		p.matches.order = append(p.matches.order, arg.id)
	}
}

// apply is clap's react after the pending argument is resolved: check the
// values, refuse a repeat, and parse.
func (p *microvmClapParser) apply(arg *microvmClapArg, values []string) error {
	switch arg.action {
	case microvmClapHelp:
		return &microvmCLIError{text: p.cmd.helpText()}
	case microvmClapVersion:
		return &microvmCLIError{text: p.cmd.version}
	case microvmClapSetTrue:
		if p.matches.present[arg.id] {
			return p.conflict(arg)
		}
		p.mark(arg)
		return nil
	}
	if len(values) == 0 {
		return p.cmd.fail(microvmClapErrorText{message: fmt.Sprintf("a value is required for '%s' but none was supplied", arg.display())})
	}
	if arg.action == microvmClapSet && p.matches.present[arg.id] {
		return p.conflict(arg)
	}
	p.mark(arg)
	for _, value := range values {
		if arg.parse != nil {
			parsed, err := arg.parse(value)
			if err != nil {
				return p.cmd.fail(microvmClapErrorText{message: fmt.Sprintf("invalid value '%s' for '%s': %s", value, arg.display(), err)})
			}
			p.matches.parsed[arg.id] = parsed
		}
		p.matches.values[arg.id] = append(p.matches.values[arg.id], value)
	}
	return nil
}

func (p *microvmClapParser) conflict(arg *microvmClapArg) error {
	return p.cmd.fail(microvmClapErrorText{
		message: fmt.Sprintf("the argument '%s' cannot be used multiple times", arg.display()),
		usage:   p.cmd.usage(nil),
	})
}

func (p *microvmClapParser) resolvePending() error {
	if p.pending == nil {
		return nil
	}
	arg, values := p.pending, p.values
	p.pending, p.values = nil, nil
	return p.apply(arg, values)
}

func (p *microvmClapParser) react(arg *microvmClapArg, values []string) error {
	if err := p.resolvePending(); err != nil {
		return err
	}
	return p.apply(arg, values)
}

func (p *microvmClapParser) start(arg *microvmClapArg) error {
	if err := p.resolvePending(); err != nil {
		return err
	}
	p.pending, p.values = arg, nil
	return nil
}

// used is the arguments given, which a smart usage line names.
func (p *microvmClapParser) used() []string {
	return append([]string(nil), p.matches.order...)
}

// didYouMean is clap's error for an unknown `--long`: a similar option of
// this command, or of a subcommand named later on the line.
func (p *microvmClapParser) didYouMean(name string, remaining []string, trailing bool) error {
	c := p.cmd
	flag, sub := "", ""
	if candidates := microvmClapDidYouMean(name, c.longs()); len(candidates) > 0 {
		flag = candidates[len(candidates)-1]
	} else {
		best := -1
		for _, subcommand := range c.subcommands {
			candidates := microvmClapDidYouMean(name, subcommand.longs())
			if len(candidates) == 0 {
				continue
			}
			score := -1
			for i, word := range remaining {
				if word == subcommand.name {
					score = i
					break
				}
			}
			if score >= 0 && (best < 0 || score < best) {
				best, flag, sub = score, candidates[len(candidates)-1], subcommand.name
			}
		}
	}
	if flag != "" && sub == "" {
		// clap adds the suggestion to the matches to build its usage line.
		if arg := c.findLong(flag); arg != nil {
			p.mark(arg)
		}
	}
	anyTrailing := false
	for _, positional := range c.positionals() {
		anyTrailing = anyTrailing || positional.trailing
	}
	text := microvmClapErrorText{message: fmt.Sprintf("unexpected argument '--%s' found", name)}
	if (flag == "" || anyTrailing) && !trailing && len(c.positionals()) > 0 {
		text.tips = append(text.tips, fmt.Sprintf("to pass '--%s' as a value, use '-- --%s'", name, name))
	}
	switch {
	case flag != "" && sub != "":
		text.tips = append(text.tips, fmt.Sprintf("'%s --%s' exists", sub, flag))
	case flag != "":
		text.similarKind, text.similar = "argument", []string{"--" + flag}
	}
	text.usage = c.usage(p.used())
	return c.fail(text)
}

// matchArgError is clap's error for a word no positional takes.
func (p *microvmClapParser) matchArgError(arg string, trailing bool) error {
	c := p.cmd
	if trailing && c.findSubcommand(arg) != nil {
		return c.fail(microvmClapErrorText{
			message: fmt.Sprintf("unexpected argument '%s' found", arg),
			tips:    []string{fmt.Sprintf("subcommand '%s' exists; to use it, remove the '--' before it", arg)},
			usage:   c.usage(nil),
		})
	}
	hasPositionals := len(c.positionals()) > 0
	suggestedTrailing := !trailing && hasPositionals && (microvmClapIsLong(arg) || microvmClapIsShort(arg))
	if len(c.subcommands) > 0 {
		names := make([]string, len(c.subcommands))
		for i, sub := range c.subcommands {
			names[i] = sub.name
		}
		if candidates := microvmClapDidYouMean(arg, names); len(candidates) > 0 {
			text := microvmClapErrorText{
				message:     fmt.Sprintf("unrecognized subcommand '%s'", arg),
				similarKind: "subcommand",
				similar:     candidates,
				usage:       c.usage(nil),
			}
			if suggestedTrailing {
				text.tips = []string{fmt.Sprintf("to pass '%s' as a value, use '%s -- %s'", arg, c.binName, arg)}
			}
			return c.fail(text)
		}
		if !hasPositionals {
			return c.unrecognizedSubcommand(arg)
		}
	}
	return p.unknownArgument(arg, suggestedTrailing)
}

func (p *microvmClapParser) unknownArgument(arg string, suggestedTrailing bool) error {
	text := microvmClapErrorText{message: fmt.Sprintf("unexpected argument '%s' found", arg), usage: p.cmd.usage(nil)}
	if suggestedTrailing {
		text.tips = []string{fmt.Sprintf("to pass '%s' as a value, use '-- %s'", arg, arg)}
	}
	return p.cmd.fail(text)
}

// helpSubcommand is clap's `help [COMMAND]...`: the help of the command
// the words name.
func (c *microvmClapCommand) helpSubcommand(names []string) error {
	target := c
	for _, name := range names {
		sub := target.findSubcommand(name)
		if sub == nil {
			return target.unrecognizedSubcommand(name)
		}
		target = sub
	}
	return &microvmCLIError{text: target.helpText()}
}

// parse parses args for the command, as clap's Parser and Validator do.
func (c *microvmClapCommand) parse(args []string) (*microvmClapMatches, error) {
	p := &microvmClapParser{cmd: c, matches: &microvmClapMatches{
		values:  map[string][]string{},
		parsed:  map[string]any{},
		present: map[string]bool{},
	}}
	positionals := c.positionals()
	position := 0
	trailing := false
	const (
		valuesDone = iota
		optionValue
		positionalValues
	)
	state := valuesDone
words:
	for i := 0; i < len(args); i++ {
		arg := args[i]
		if !trailing {
			if state == valuesDone {
				if sub := c.findSubcommand(arg); sub != nil {
					if sub.isHelp {
						return nil, c.helpSubcommand(args[i+1:])
					}
					matches, err := sub.parse(args[i+1:])
					if err != nil {
						return nil, err
					}
					p.matches.sub = &microvmClapSubcommand{name: sub.name, matches: matches}
					break words
				}
			}
			// The positional this word would fill takes hyphenated words.
			hyphenPositional := position < len(positionals) && positionals[position].trailing
			switch {
			case arg == "--":
				trailing = true
				continue
			case strings.HasPrefix(arg, "--"):
				name, value, attached := strings.Cut(arg[2:], "=")
				if found := c.findLong(name); found != nil {
					var err error
					switch {
					case found.takesValue() && attached:
						err = p.react(found, []string{value})
						state = valuesDone
					case found.takesValue():
						err = p.start(found)
						state = optionValue
					case attached:
						_ = p.resolvePending()
						var used []string
						for _, id := range p.matches.order {
							if given := c.find(id); given != nil && !given.required {
								used = append(used, id)
							}
						}
						used = append(used, found.id)
						return nil, c.fail(microvmClapErrorText{
							message: fmt.Sprintf("unexpected value '%s' for '%s' found; no more were expected", value, found.display()),
							usage:   c.usage(used),
						})
					default:
						err = p.react(found, nil)
						state = valuesDone
					}
					if err != nil {
						return nil, err
					}
					continue
				}
				if !hyphenPositional {
					_ = p.resolvePending()
					return nil, p.didYouMean(name, args[i+1:], trailing)
				}
			case len(arg) > 1 && arg[0] == '-':
				cluster := arg[1:]
				unknown := false
				for _, r := range cluster {
					unknown = unknown || c.findShort(r) == nil
				}
				if hyphenPositional && unknown {
					break
				}
				for j, r := range cluster {
					found := c.findShort(r)
					if found == nil {
						_ = p.resolvePending()
						return nil, p.unknownArgument("-"+string(r), !trailing && len(positionals) > 0)
					}
					if !found.takesValue() {
						if err := p.react(found, nil); err != nil {
							return nil, err
						}
						state = valuesDone
						continue
					}
					rest := cluster[j+utf8.RuneLen(r):]
					var err error
					if rest != "" {
						err = p.react(found, []string{strings.TrimPrefix(rest, "=")})
						state = valuesDone
					} else {
						err = p.start(found)
						state = optionValue
					}
					if err != nil {
						return nil, err
					}
					break
				}
				continue
			}
			if state == optionValue {
				// The value of the option before it.
				p.values = append(p.values, arg)
				state = valuesDone
				continue
			}
		}
		if position < len(positionals) {
			positional := positionals[position]
			if positional.trailing {
				trailing = true
			}
			if p.pending != positional {
				if err := p.start(positional); err != nil {
					return nil, err
				}
			}
			p.values = append(p.values, arg)
			if positional.multiple {
				state = positionalValues
			} else {
				position++
				state = valuesDone
			}
			continue
		}
		_ = p.resolvePending()
		return nil, p.matchArgError(arg, trailing)
	}
	if err := p.resolvePending(); err != nil {
		return nil, err
	}
	return p.matches, p.validate()
}

// validate is clap's Validator: a command with subcommands and nothing
// given shows its help, and every required argument must be present.
func (p *microvmClapParser) validate() error {
	c := p.cmd
	if len(c.subcommands) > 0 && p.matches.sub == nil && len(p.matches.order) == 0 {
		return &microvmCLIError{text: c.helpText(), useStderr: true}
	}
	positionals := c.positionals()
	indexOf := func(arg *microvmClapArg) int {
		for i, positional := range positionals {
			if positional == arg {
				return i
			}
		}
		return -1
	}
	var missing []*microvmClapArg
	highest := -1
	for _, arg := range c.args {
		if arg.required && !p.matches.present[arg.id] {
			missing = append(missing, arg)
			if arg.positional {
				highest = max(highest, indexOf(arg))
			}
		}
	}
	// For display, every preceding positional that is absent is named too.
	for i, positional := range positionals {
		if i < highest && !p.matches.present[positional.id] {
			missing = append(missing, positional)
		}
	}
	if len(missing) == 0 {
		return nil
	}
	// clap lists the absent required options, then the absent positionals.
	var options []string
	seen := map[string]bool{}
	slots := map[*microvmClapArg]bool{}
	consider := func(arg *microvmClapArg) {
		if p.matches.present[arg.id] {
			return
		}
		if arg.positional {
			slots[arg] = true
		} else if text := arg.stylized(true); !seen[text] {
			seen[text] = true
			options = append(options, text)
		}
	}
	for _, arg := range c.args {
		if arg.required {
			consider(arg)
		}
	}
	for _, arg := range missing {
		consider(arg)
	}
	message := "the following required arguments were not provided:"
	for _, option := range options {
		message += "\n  " + option
	}
	for _, positional := range positionals {
		if slots[positional] {
			message += "\n  " + positional.stylized(true)
		}
	}
	used := p.used()
	for _, arg := range missing {
		used = append(used, arg.id)
	}
	return c.fail(microvmClapErrorText{message: message, usage: c.usage(used)})
}

// ---------------------------------------------------------------------------
// Value parsers: clap's for f64, u64 and u32, with Rust's error text.

func microvmClapParseF64(text string) (any, error) {
	return microvmParseRustF64(text)
}

// microvmParseRustF64 parses text as Rust's `f64::from_str` does: decimal
// digits with an optional fraction and exponent, or `inf`, `infinity` and
// `nan` in any case, each with an optional sign; no hexadecimal, no
// underscores, and a magnitude too large is infinity, not an error.
func microvmParseRustF64(text string) (float64, error) {
	if text == "" {
		return 0, errors.New("cannot parse float from empty string")
	}
	invalid := errors.New("invalid float literal")
	if strings.ContainsAny(text, "xXpP_") {
		return 0, invalid
	}
	value, err := strconv.ParseFloat(text, 64)
	if err != nil {
		var numeric *strconv.NumError
		if errors.As(err, &numeric) && numeric.Err == strconv.ErrRange {
			return value, nil
		}
		return 0, invalid
	}
	return value, nil
}

func microvmClapParseU64(text string) (any, error) {
	return microvmParseUint(text, 64)
}

// microvmClapParseU32 is clap's u32 parser: an i64, then the range.
func microvmClapParseU32(text string) (any, error) {
	value, err := microvmParseInt64(text)
	if err != nil {
		return nil, err
	}
	if value < 0 || value > 1<<32-1 {
		return nil, fmt.Errorf("%d is not in 0..=4294967295", value)
	}
	return uint32(value), nil
}
