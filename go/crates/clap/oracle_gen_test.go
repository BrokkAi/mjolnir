// A deterministic generator of commands (in the oracle's JSON form) and of
// argument vectors aimed at them: valid uses, every kind of mistake, help
// and version at each level, near-miss names.

package clap

import (
	"math/rand/v2"
	"strconv"
	"strings"
)

type genOpt struct {
	id            string
	short         rune
	long          string
	takesValue    bool
	values        []string
	delim         rune
	positional    bool
	multiple      bool
	last          bool
	requireEquals bool
	aliases       []string
	env           string
	global        bool
	required      bool
	ok            []string
}

type genCmd struct {
	name      string
	aliases   []string
	shortFlag rune
	longFlag  string
	opts      []*genOpt
	subs      []*genCmd
	external  bool
	disableH  bool
}

type specGen struct {
	r       *rand.Rand
	ids     int
	shorts  []rune
	longs   []string
	usedLng map[string]bool
	usedSh  map[rune]bool
	envs    []string
}

func newSpecGen(seed uint64) *specGen {
	return &specGen{
		r:       rand.New(rand.NewPCG(seed, seed^0x9e3779b97f4a7c15)),
		shorts:  []rune("abcdefgijklmnopqrstuvwxyzABCDEFGIJKLMNOPQRSTUWXYZ0123"),
		usedLng: map[string]bool{},
		usedSh:  map[rune]bool{},
	}
}

func (g *specGen) chance(p float64) bool { return g.r.Float64() < p }
func (g *specGen) intn(n int) int        { return g.r.IntN(n) }

func pick[T any](g *specGen, items []T) T { return items[g.r.IntN(len(items))] }

var genWords = []string{"alpha", "beta", "gamma", "delta", "config", "output", "input", "verbose", "quiet",
	"name", "path", "level", "color", "mode", "target", "profile", "session", "workspace", "json", "force",
	"dry-run", "all", "limit", "after-seq", "role", "branch", "base", "title", "model", "effort", "kind",
	"root", "spec", "socket", "event", "server", "list", "temp", "length", "overwrite", "repository"}

var genHelpWords = []string{"the", "a", "value", "used", "when", "set", "to", "print", "output", "files",
	"session", "for", "each", "with", "this", "is", "an", "option", "that", "does", "nothing", "useful",
	"and", "more", "words", "to", "make", "lines", "long", "enough", "wrap"}

func (g *specGen) newId() string {
	g.ids++
	return pick(g, genWords[:20]) + "_" + strconv.Itoa(g.ids)
}

func (g *specGen) newLong() string {
	for {
		l := pick(g, genWords)
		if g.chance(0.3) {
			l += "-" + pick(g, genWords)
		}
		if !g.usedLng[l] {
			g.usedLng[l] = true
			return l
		}
		if g.chance(0.5) {
			l = l + strconv.Itoa(g.intn(9))
			if !g.usedLng[l] {
				g.usedLng[l] = true
				return l
			}
		}
	}
}

func (g *specGen) newShort() rune {
	for tries := 0; tries < 20; tries++ {
		s := pick(g, g.shorts)
		if !g.usedSh[s] {
			g.usedSh[s] = true
			return s
		}
	}
	return 0
}

func (g *specGen) helpText(long bool) string {
	n := 2 + g.intn(6)
	if long {
		n = 18 + g.intn(20)
	}
	words := make([]string, n)
	for i := range words {
		words[i] = pick(g, genHelpWords)
	}
	text := strings.Join(words, " ")
	if g.chance(0.1) {
		text += "."
	}
	if g.chance(0.05) {
		text += "{n}second line"
	}
	if g.chance(0.05) {
		text += "\nthird line"
	}
	return text
}

func call(method string, params ...any) []any { return append([]any{method}, params...) }

// valueKind is a value parser with values to try on it: the first ok of
// them are accepted, the others are not.
//
// No value lies between 2^63 and 2^64: a usize is a Go int in the port, so
// such a value, which Rust accepts for a usize argument, is refused in Go
// (doc.go says so).
type valueKind struct {
	vp     any
	values []string
	ok     int
}

var genValueKinds = []valueKind{
	{nil, []string{"x", "foo", "a,b", "日本", "a b", "", "-1"}, 5},
	{"string", []string{"x", "foo", "bar,baz", "é", "-5"}, 4},
	{"u8", []string{"0", "7", "255", "256", "-1", "x", "3.5"}, 3},
	{"i64", []string{"-3", "12", "9223372036854775807", "-9223372036854775809", "z"}, 3},
	{map[string]any{"i64_range": []any{-5, 10}}, []string{"10", "4", "11", "-6", "-5"}, 2},
	{map[string]any{"u64_range": []any{1, nil}}, []string{"1", "9223372036854775807", "0", "18446744073709551616"}, 2},
	{"bool", []string{"true", "false", "yes", "1", "TRUE"}, 2},
	{"usize", []string{"0", "5", "-5", "99999999999999999999"}, 2},
	{"f64", []string{"1.5", "1e3", "inf", "nan", "-0.25", "x"}, 4},
	{"color", []string{"auto", "always", "never", "Never", "neve", "colour"}, 3},
	{"mode", []string{"fast", "slow", "sl", "very-careful", "secret", "unused", "Fast", "slo"}, 5},
	{"fn_even", []string{"2", "8", "3", "x", ""}, 2},
	{"fn_upper", []string{"abc", "Mixed", ""}, 2},
	{map[string]any{"possible": []any{"red", "green", "blue"}}, []string{"red", "green", "bleu", "RED", "yellow"}, 2},
	{map[string]any{"possible": []any{map[string]any{"name": "one", "help": "the first"}, map[string]any{"name": "two", "aliases": []any{"2"}}, map[string]any{"name": "three", "hide": true}}}, []string{"one", "two", "2", "three", "thre"}, 4},
	{"boolish", []string{"yes", "no", "on", "off", "maybe"}, 4},
	{"falsey", []string{"false", "0", "n", "anything"}, 4},
	{"non_empty_string", []string{"x", ""}, 1},
	{"path", []string{"/tmp/x", "rel/p", "./a b"}, 3},
}

// genOption adds an option or flag to calls and model.
func (g *specGen) genOption(cmd *genCmd, global bool) []any {
	opt := &genOpt{id: g.newId(), global: global}
	var calls []any
	hasLong := g.chance(0.85)
	hasShort := !hasLong || g.chance(0.5)
	if hasShort {
		if s := g.newShort(); s != 0 {
			opt.short = s
			calls = append(calls, call("short", string(s)))
		} else if !hasLong {
			hasLong = true
		}
	}
	if hasLong {
		opt.long = g.newLong()
		calls = append(calls, call("long", opt.long))
	}
	kind := g.intn(10)
	switch {
	case kind < 4:
		// a flag
		action := pick(g, []string{"set_true", "set_true", "set_false", "count"})
		calls = append(calls, call("action", action))
	default:
		opt.takesValue = true
		vk := pick(g, genValueKinds)
		opt.values = vk.values
		opt.ok = vk.values[:vk.ok]
		if vk.vp != nil {
			calls = append(calls, call("value_parser", vk.vp))
		}
		action := "set"
		if g.chance(0.3) {
			action = "append"
			opt.multiple = true
		}
		if g.chance(0.6) {
			calls = append(calls, call("action", action))
		} else if action == "append" {
			calls = append(calls, call("action", action))
		}
		if g.chance(0.2) {
			r := pick(g, []any{2, []any{1, nil}, []any{0, 1}, []any{1, 3}, []any{0, nil}})
			calls = append(calls, call("num_args", r))
			opt.multiple = true
		} else if g.chance(0.15) {
			opt.requireEquals = true
			calls = append(calls, call("require_equals", true))
			if g.chance(0.5) {
				calls = append(calls, call("num_args", []any{0, 1}), call("default_missing_value", pick(g, vk.values)))
			}
		}
		if g.chance(0.4) {
			calls = append(calls, call("value_name", strings.ToUpper(pick(g, genWords))))
		}
		if g.chance(0.25) {
			calls = append(calls, call("default_value", pick(g, vk.values)))
		}
		if g.chance(0.1) {
			opt.delim = ','
			calls = append(calls, call("value_delimiter", ","))
		}
		if g.chance(0.1) {
			calls = append(calls, call("allow_hyphen_values", true))
		}
		if g.chance(0.1) {
			calls = append(calls, call("allow_negative_numbers", true))
		}
		if g.chance(0.08) {
			calls = append(calls, call("hide_possible_values", true))
		}
		if g.chance(0.08) {
			calls = append(calls, call("hide_default_value", true))
		}
		if g.chance(0.08) {
			calls = append(calls, call("ignore_case", true))
		}
		if g.chance(0.15) && !global {
			opt.required = true
			calls = append(calls, call("required", true))
		}
	}
	if g.chance(0.15) {
		opt.env = "CLAP_ORACLE_" + strings.ToUpper(opt.id)
		calls = append(calls, call("env", opt.env))
		g.envs = append(g.envs, opt.env)
		if g.chance(0.3) {
			calls = append(calls, call("hide_env_values", true))
		}
	}
	if g.chance(0.8) {
		calls = append(calls, call("help", g.helpText(g.chance(0.15))))
	}
	if g.chance(0.15) {
		calls = append(calls, call("long_help", g.helpText(true)+"\n\n"+g.helpText(false)))
	}
	if g.chance(0.1) && hasLong {
		alias := g.newLong()
		opt.aliases = append(opt.aliases, alias)
		calls = append(calls, call(pick(g, []string{"alias", "visible_alias"}), alias))
	}
	if g.chance(0.08) {
		if s := g.newShort(); s != 0 {
			calls = append(calls, call(pick(g, []string{"short_alias", "visible_short_alias"}), string(s)))
		}
	}
	if g.chance(0.08) {
		calls = append(calls, call("hide", true))
	}
	if g.chance(0.05) {
		calls = append(calls, call("hide_short_help", true))
	}
	if g.chance(0.05) {
		calls = append(calls, call("hide_long_help", true))
	}
	if g.chance(0.08) {
		calls = append(calls, call("next_line_help", true))
	}
	if g.chance(0.1) {
		calls = append(calls, call("display_order", g.intn(5)))
	}
	if g.chance(0.1) {
		calls = append(calls, call("help_heading", pick(g, []any{"Advanced", "Output", nil})))
	}
	if global {
		calls = append(calls, call("global", true))
	}
	cmd.opts = append(cmd.opts, opt)
	return call("arg", map[string]any{"id": opt.id, "calls": calls})
}

// genPositionals adds positionals that satisfy clap's assertions: the
// required ones come first, and only the last may take several values.
func (g *specGen) genPositionals(cmd *genCmd, hasSubs bool) []any {
	n := g.intn(4)
	required := 0
	if n > 0 {
		required = g.intn(n + 1)
	}
	var calls []any
	for i := 0; i < n; i++ {
		opt := &genOpt{id: g.newId(), positional: true, takesValue: true}
		var argCalls []any
		vk := pick(g, genValueKinds[:6])
		opt.values = vk.values
		opt.ok = vk.values[:vk.ok]
		if vk.vp != nil {
			argCalls = append(argCalls, call("value_parser", vk.vp))
		}
		isLast := i == n-1
		if i < required && !(isLast && hasSubs && g.chance(0.5)) {
			opt.required = true
			argCalls = append(argCalls, call("required", true))
		}
		if isLast && g.chance(0.4) {
			opt.multiple = true
			argCalls = append(argCalls, call("num_args", []any{1, nil}))
			if g.chance(0.4) {
				argCalls = append(argCalls, call("trailing_var_arg", true))
				if g.chance(0.5) {
					argCalls = append(argCalls, call("allow_hyphen_values", true))
				}
			}
		} else if isLast && n > 1 && g.chance(0.2) && !(i < required && hasSubs) {
			opt.last = true
			argCalls = append(argCalls, call("last", true))
		}
		if g.chance(0.5) {
			argCalls = append(argCalls, call("value_name", strings.ToUpper(pick(g, genWords))))
		}
		if g.chance(0.7) {
			argCalls = append(argCalls, call("help", g.helpText(g.chance(0.1))))
		}
		if g.chance(0.15) && i >= required {
			argCalls = append(argCalls, call("default_value", pick(g, vk.values)))
		}
		cmd.opts = append(cmd.opts, opt)
		calls = append(calls, call("arg", map[string]any{"id": opt.id, "calls": argCalls}))
	}
	return calls
}

// genRelations adds requires, conflicts and conditional requirements
// between the options of one command.
func (g *specGen) genRelations(cmd *genCmd) []any {
	var calls []any
	var named []*genOpt
	for _, o := range cmd.opts {
		// A global argument is copied into every subcommand, where the
		// ids it would name do not exist.
		if !o.positional && !o.global {
			named = append(named, o)
		}
	}
	if len(named) < 2 {
		return nil
	}
	for i := 0; i < 1+g.intn(3); i++ {
		a := pick(g, named)
		b := pick(g, named)
		if a == b {
			continue
		}
		switch g.intn(6) {
		case 0:
			calls = append(calls, call("mut_arg", a.id, []any{call("requires", b.id)}))
		case 1:
			calls = append(calls, call("mut_arg", a.id, []any{call("conflicts_with", b.id)}))
		case 2:
			calls = append(calls, call("mut_arg", a.id, []any{call("overrides_with", b.id)}))
		case 3:
			if b.takesValue && len(b.values) > 0 {
				calls = append(calls, call("mut_arg", a.id, []any{call("required", false), call("required_if_eq", b.id, b.values[0])}))
			}
		case 4:
			calls = append(calls, call("mut_arg", a.id, []any{call("required", false), call("required_unless_present", b.id)}))
		case 5:
			if a.takesValue {
				calls = append(calls, call("mut_arg", a.id, []any{call("default_value_if", b.id, "present", "from-if")}))
			}
		}
	}
	if len(named) >= 3 && g.chance(0.3) {
		group := map[string]any{"id": "grp_" + strconv.Itoa(g.ids), "calls": []any{
			call("args", []any{named[0].id, named[1].id}),
			call("required", g.chance(0.5)),
			call("multiple", g.chance(0.5)),
		}}
		calls = append(calls, call("group", group))
	}
	return calls
}

var genTemplates = []string{
	"{name} {version}\n{about-section}\n{usage-heading} {usage}\n\n{all-args}{after-help}",
	"{bin} {author-with-newline}{about}\n\nUSAGE:\n{tab}{usage}\n\nOPTIONS:\n{options}\n\nARGS:\n{positionals}\n\nCOMMANDS:\n{subcommands}",
	"{before-help}{usage}{unknown-tag}{after-help}",
}

func (g *specGen) genCommand(name string, depth int) (map[string]any, *genCmd) {
	cmd := &genCmd{name: name}
	var calls []any
	if g.chance(0.6) {
		calls = append(calls, call("about", g.helpText(g.chance(0.2))))
	}
	if g.chance(0.15) {
		calls = append(calls, call("long_about", g.helpText(true)+"\n\n"+g.helpText(true)))
	}
	hasVersion := depth == 0 && g.chance(0.5)
	if hasVersion {
		calls = append(calls, call("version", pick(g, []string{"1.0.0", "0.3.1-beta", "2.45.0"})))
		if g.chance(0.2) {
			calls = append(calls, call("long_version", "2.45.0 (abc123 2026-11-20)"))
		}
		if g.chance(0.3) {
			calls = append(calls, call("propagate_version", true))
		}
	}
	if g.chance(0.1) {
		calls = append(calls, call("author", "Somebody <s@example.com>"))
	}
	if g.chance(0.1) {
		calls = append(calls, call("after_help", g.helpText(false)))
	}
	if g.chance(0.05) {
		calls = append(calls, call("after_long_help", g.helpText(true)))
	}
	if g.chance(0.1) {
		calls = append(calls, call("before_help", g.helpText(false)))
	}
	if depth == 0 {
		if g.chance(0.4) {
			calls = append(calls, call("term_width", pick(g, []int{0, 30, 50, 80, 120, 200})))
		} else if g.chance(0.1) {
			calls = append(calls, call("max_term_width", pick(g, []int{40, 60})))
		}
		calls = append(calls, call("styles", pick(g, []string{"plain", "styled", "styled", "custom"})))
		if g.chance(0.2) {
			calls = append(calls, call("color", pick(g, []string{"always", "never", "auto"})))
		}
		if g.chance(0.1) {
			calls = append(calls, call("next_line_help", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("help_template", pick(g, genTemplates)))
		}
		if g.chance(0.05) {
			calls = append(calls, call("override_usage", "prog [weird] <usage>"))
		}
		if g.chance(0.08) {
			calls = append(calls, call("disable_help_flag", true))
			cmd.disableH = true
		}
		if g.chance(0.08) {
			calls = append(calls, call("infer_long_args", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("args_override_self", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("dont_delimit_trailing_values", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("hide_possible_values", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("ignore_errors", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("bin_name", "my-prog"))
		}
	}
	if g.chance(0.05) {
		calls = append(calls, call("display_name", "displayed"))
	}

	hasSubs := depth < 2 && g.chance(0.5-float64(depth)*0.25)
	nOpts := g.intn(6)
	for i := 0; i < nOpts; i++ {
		if i == 2 && g.chance(0.3) {
			calls = append(calls, call("next_help_heading", pick(g, []any{"Section A", "Section B", nil})))
		}
		calls = append(calls, g.genOption(cmd, depth == 0 && hasSubs && g.chance(0.2)))
	}
	if !(hasSubs && g.chance(0.5)) {
		calls = append(calls, g.genPositionals(cmd, hasSubs)...)
	}
	calls = append(calls, g.genRelations(cmd)...)

	if hasSubs {
		n := 1 + g.intn(3)
		for i := 0; i < n; i++ {
			subName := pick(g, genWords) + strconv.Itoa(g.intn(3))
			dup := false
			for _, s := range cmd.subs {
				if s.name == subName {
					dup = true
				}
			}
			if dup {
				continue
			}
			subSpec, sub := g.genCommand(subName, depth+1)
			subCalls := subSpec["calls"].([]any)
			if g.chance(0.2) {
				alias := subName + "-alias"
				sub.aliases = append(sub.aliases, alias)
				subCalls = append(subCalls, call(pick(g, []string{"alias", "visible_alias"}), alias))
			}
			if g.chance(0.1) {
				if s := g.newShort(); s != 0 {
					sub.shortFlag = s
					subCalls = append(subCalls, call("short_flag", string(s)))
				}
			}
			if g.chance(0.1) {
				sub.longFlag = g.newLong()
				subCalls = append(subCalls, call("long_flag", sub.longFlag))
			}
			if g.chance(0.1) {
				subCalls = append(subCalls, call("hide", true))
			}
			if g.chance(0.1) {
				subCalls = append(subCalls, call("display_order", g.intn(3)))
			}
			subSpec["calls"] = subCalls
			cmd.subs = append(cmd.subs, sub)
			calls = append(calls, call("subcommand", subSpec))
		}
		if g.chance(0.25) {
			calls = append(calls, call("subcommand_required", true))
		}
		if g.chance(0.15) {
			calls = append(calls, call("arg_required_else_help", true))
		}
		if g.chance(0.1) {
			calls = append(calls, call("infer_subcommands", true))
		}
		if g.chance(0.1) {
			calls = append(calls, call("args_conflicts_with_subcommands", true))
		}
		if g.chance(0.1) {
			calls = append(calls, call("subcommand_negates_reqs", true))
		}
		if g.chance(0.05) {
			calls = append(calls, call("subcommand_precedence_over_arg", true))
		}
		if g.chance(0.08) {
			calls = append(calls, call("disable_help_subcommand", true))
		}
		if g.chance(0.08) {
			calls = append(calls, call("flatten_help", true))
		}
		if g.chance(0.08) {
			calls = append(calls, call("subcommand_value_name", "SUBCMD"))
		}
		if g.chance(0.08) {
			calls = append(calls, call("subcommand_help_heading", "Tasks"))
		}
		if g.chance(0.08) {
			cmd.external = true
			calls = append(calls, call("allow_external_subcommands", true))
			if g.chance(0.5) {
				calls = append(calls, call("external_subcommand_value_parser", "string"))
			}
		}
	}
	return map[string]any{"name": name, "calls": calls}, cmd
}

func nearMiss(g *specGen, s string) string {
	if len(s) < 2 {
		return s + "x"
	}
	b := []byte(s)
	switch g.intn(3) {
	case 0:
		i := g.intn(len(b) - 1)
		b[i], b[i+1] = b[i+1], b[i]
	case 1:
		i := g.intn(len(b))
		b = append(b[:i], b[i+1:]...)
	default:
		i := g.intn(len(b))
		b = append(b[:i], append([]byte{'e'}, b[i:]...)...)
	}
	return string(b)
}

func (g *specGen) valueFor(o *genOpt) string {
	if len(o.values) > 0 && g.chance(0.8) {
		return pick(g, o.values)
	}
	return pick(g, []string{"v", "-v", "--", "", "日本語", "x,y", "-1"})
}

// genArgv builds one argument vector for cmd.
func (g *specGen) genArgv(root *genCmd) []string {
	argv := []string{"prog"}
	cmd := root
	n := g.intn(7)
	for i := 0; i < n; i++ {
		var named []*genOpt
		var positional []*genOpt
		for _, o := range cmd.opts {
			if o.positional {
				positional = append(positional, o)
			} else {
				named = append(named, o)
			}
		}
		switch k := g.intn(20); {
		case k < 7 && len(named) > 0:
			o := pick(g, named)
			useLong := o.long != "" && (o.short == 0 || g.chance(0.6))
			if !o.takesValue {
				if useLong {
					argv = append(argv, "--"+o.long)
					if g.chance(0.05) {
						argv[len(argv)-1] += "=true"
					}
				} else {
					flags := "-" + string(o.short)
					if g.chance(0.3) {
						for _, other := range named {
							if other.short != 0 && !other.takesValue && g.chance(0.5) {
								flags += string(other.short)
							}
						}
					}
					argv = append(argv, flags)
				}
				continue
			}
			v := g.valueFor(o)
			form := g.intn(4)
			if useLong {
				switch {
				case form == 0 || o.requireEquals:
					argv = append(argv, "--"+o.long+"="+v)
				case form == 1 && g.chance(0.3):
					argv = append(argv, "--"+o.long)
				default:
					argv = append(argv, "--"+o.long, v)
				}
			} else {
				switch form {
				case 0:
					argv = append(argv, "-"+string(o.short)+v)
				case 1:
					argv = append(argv, "-"+string(o.short)+"="+v)
				default:
					argv = append(argv, "-"+string(o.short), v)
				}
			}
			if o.multiple && g.chance(0.4) {
				argv = append(argv, g.valueFor(o))
			}
		case k < 10 && len(positional) > 0:
			argv = append(argv, g.valueFor(pick(g, positional)))
		case k < 12 && len(cmd.subs) > 0:
			sub := pick(g, cmd.subs)
			switch {
			case g.chance(0.6):
				argv = append(argv, sub.name)
			case len(sub.aliases) > 0 && g.chance(0.5):
				argv = append(argv, sub.aliases[0])
			case sub.shortFlag != 0 && g.chance(0.5):
				argv = append(argv, "-"+string(sub.shortFlag))
			case sub.longFlag != "" && g.chance(0.5):
				argv = append(argv, "--"+sub.longFlag)
			default:
				argv = append(argv, nearMiss(g, sub.name))
				continue
			}
			cmd = sub
		case k == 12:
			argv = append(argv, pick(g, []string{"--help", "-h", "help"}))
			if len(cmd.subs) > 0 && argv[len(argv)-1] == "help" && g.chance(0.5) {
				argv = append(argv, pick(g, cmd.subs).name)
			}
		case k == 13:
			argv = append(argv, pick(g, []string{"--version", "-V"}))
		case k == 14:
			argv = append(argv, "--")
		case k == 15 && len(named) > 0:
			o := pick(g, named)
			if o.long != "" {
				argv = append(argv, "--"+nearMiss(g, o.long))
			} else {
				argv = append(argv, "-Q")
			}
		case k == 16:
			argv = append(argv, pick(g, []string{"--unknown", "-Z", "-", "-5", "--x=y", "", "-é"}))
		default:
			argv = append(argv, pick(g, []string{"word", "file.txt", "-3", "日本"}))
		}
	}
	return argv
}

// genEnv sets some of the variables the command's options read.
func (g *specGen) genEnv() map[string]string {
	env := map[string]string{}
	for _, name := range g.envs {
		if g.chance(0.5) {
			env[name] = pick(g, []string{"from-env", "7", "true", "fast", "", "red"})
		}
	}
	return env
}

// okValue is a value the option's parser accepts.
func (g *specGen) okValue(o *genOpt) string {
	if len(o.ok) > 0 {
		return pick(g, o.ok)
	}
	return "v"
}

// genValidArgv builds an argument vector that gives every required
// argument a value the parser accepts, adds some optional ones, and may go
// into a subcommand: mostly parses that succeed.
func (g *specGen) genValidArgv(root *genCmd) []string {
	argv := []string{"prog"}
	cmd := root
	for depth := 0; depth < 3; depth++ {
		var positionals []string
		for _, o := range cmd.opts {
			if o.positional {
				if o.required || g.chance(0.4) {
					positionals = append(positionals, g.okValue(o))
					if o.multiple && g.chance(0.5) {
						positionals = append(positionals, g.okValue(o))
					}
				}
				continue
			}
			if !o.required && !g.chance(0.3) {
				continue
			}
			name := "--" + o.long
			if o.long == "" {
				name = "-" + string(o.short)
			}
			if !o.takesValue {
				argv = append(argv, name)
				continue
			}
			if o.requireEquals || g.chance(0.3) {
				argv = append(argv, name+"="+g.okValue(o))
			} else {
				argv = append(argv, name, g.okValue(o))
			}
		}
		if len(positionals) > 0 {
			if g.chance(0.2) {
				argv = append(argv, "--")
			}
			argv = append(argv, positionals...)
			break
		}
		if len(cmd.subs) == 0 || !g.chance(0.7) {
			break
		}
		cmd = pick(g, cmd.subs)
		argv = append(argv, cmd.name)
	}
	return argv
}
