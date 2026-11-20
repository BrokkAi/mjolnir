// The Go side of the oracle protocol for clap (go/port/oracle/src/clap_oracle.rs):
// a command described as JSON is built with this package's builder, run, and
// the result dumped in the shape the Rust module replies with.

package clap

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"strconv"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// specCmd, specArg and specGroup are the CMD, ARG and GROUP of the protocol.
type specCmd struct {
	Name  string            `json:"name"`
	Calls []json.RawMessage `json:"calls"`
}

type specArg struct {
	Id    string            `json:"id"`
	Calls []json.RawMessage `json:"calls"`
}

type specGroup struct {
	Id    string            `json:"id"`
	Calls []json.RawMessage `json:"calls"`
}

// specParams is one CALL: the method name and its parameters.
type specParams struct {
	method string
	params []json.RawMessage
}

func decodeCall(raw json.RawMessage) (specParams, error) {
	var parts []json.RawMessage
	if err := json.Unmarshal(raw, &parts); err != nil || len(parts) == 0 {
		return specParams{}, fmt.Errorf("call is not a non-empty array: %s", raw)
	}
	var method string
	if err := json.Unmarshal(parts[0], &method); err != nil {
		return specParams{}, fmt.Errorf("call method is not a string: %s", raw)
	}
	return specParams{method: method, params: parts[1:]}, nil
}

func (p specParams) str(i int) (string, error) {
	var s string
	if i >= len(p.params) {
		return "", fmt.Errorf("%s: missing parameter %d", p.method, i)
	}
	err := json.Unmarshal(p.params[i], &s)
	return s, err
}

func (p specParams) optStr(i int) (rs.Option[string], error) {
	var s *string
	if i >= len(p.params) {
		return rs.None[string](), fmt.Errorf("%s: missing parameter %d", p.method, i)
	}
	if err := json.Unmarshal(p.params[i], &s); err != nil {
		return rs.None[string](), err
	}
	return rs.OptionFromPtr(s), nil
}

func (p specParams) strs(i int) ([]string, error) {
	var s []string
	if i >= len(p.params) {
		return nil, fmt.Errorf("%s: missing parameter %d", p.method, i)
	}
	err := json.Unmarshal(p.params[i], &s)
	return s, err
}

func (p specParams) boolean(i int) (bool, error) {
	var b bool
	if i >= len(p.params) {
		return false, fmt.Errorf("%s: missing parameter %d", p.method, i)
	}
	err := json.Unmarshal(p.params[i], &b)
	return b, err
}

func (p specParams) num(i int) (int, error) {
	var n int
	if i >= len(p.params) {
		return 0, fmt.Errorf("%s: missing parameter %d", p.method, i)
	}
	err := json.Unmarshal(p.params[i], &n)
	return n, err
}

func (p specParams) optNum(i int) (rs.Option[int], error) {
	var n *int
	if i >= len(p.params) {
		return rs.None[int](), fmt.Errorf("%s: missing parameter %d", p.method, i)
	}
	if err := json.Unmarshal(p.params[i], &n); err != nil {
		return rs.None[int](), err
	}
	return rs.OptionFromPtr(n), nil
}

func (p specParams) char(i int) (rune, error) {
	s, err := p.str(i)
	if err != nil {
		return 0, err
	}
	r, size := utf8.DecodeRuneInString(s)
	if size == 0 || size != len(s) {
		return 0, fmt.Errorf("%s: not one character: %q", p.method, s)
	}
	return r, nil
}

func (p specParams) runes(i int) ([]rune, error) {
	strs, err := p.strs(i)
	if err != nil {
		return nil, err
	}
	var out []rune
	for _, s := range strs {
		r, size := utf8.DecodeRuneInString(s)
		if size == 0 || size != len(s) {
			return nil, fmt.Errorf("%s: not one character: %q", p.method, s)
		}
		out = append(out, r)
	}
	return out, nil
}

func specBuildCommand(raw json.RawMessage) (*Command, error) {
	var spec specCmd
	if err := json.Unmarshal(raw, &spec); err != nil {
		return nil, err
	}
	cmd := CommandNew(spec.Name)
	return specApplyCommandCalls(cmd, spec.Calls)
}

func specApplyCommandCalls(cmd *Command, calls []json.RawMessage) (*Command, error) {
	for _, raw := range calls {
		call, err := decodeCall(raw)
		if err != nil {
			return nil, err
		}
		cmd, err = specApplyCommandCall(cmd, call)
		if err != nil {
			return nil, err
		}
	}
	return cmd, nil
}

var specCommandStringSetters = map[string]func(*Command, string) *Command{
	"about":                   (*Command).About,
	"long_about":              (*Command).LongAbout,
	"version":                 (*Command).Version,
	"long_version":            (*Command).LongVersion,
	"author":                  (*Command).Author,
	"bin_name":                (*Command).BinName,
	"display_name":            (*Command).DisplayName,
	"before_help":             (*Command).BeforeHelp,
	"before_long_help":        (*Command).BeforeLongHelp,
	"after_help":              (*Command).AfterHelp,
	"after_long_help":         (*Command).AfterLongHelp,
	"override_usage":          (*Command).OverrideUsage,
	"override_help":           (*Command).OverrideHelp,
	"help_template":           (*Command).HelpTemplate,
	"subcommand_value_name":   (*Command).SubcommandValueName,
	"subcommand_help_heading": (*Command).SubcommandHelpHeading,
	"alias":                   (*Command).Alias,
	"visible_alias":           (*Command).VisibleAlias,
	"long_flag":               (*Command).LongFlag,
	"long_flag_alias":         (*Command).LongFlagAlias,
	"visible_long_flag_alias": (*Command).VisibleLongFlagAlias,
}

var specCommandBoolSetters = map[string]func(*Command, bool) *Command{
	"no_binary_name":                  (*Command).NoBinaryName,
	"ignore_errors":                   (*Command).IgnoreErrors,
	"args_override_self":              (*Command).ArgsOverrideSelf,
	"dont_delimit_trailing_values":    (*Command).DontDelimitTrailingValues,
	"propagate_version":               (*Command).PropagateVersion,
	"next_line_help":                  (*Command).NextLineHelp,
	"disable_help_flag":               (*Command).DisableHelpFlag,
	"disable_version_flag":            (*Command).DisableVersionFlag,
	"disable_help_subcommand":         (*Command).DisableHelpSubcommand,
	"disable_colored_help":            (*Command).DisableColoredHelp,
	"help_expected":                   (*Command).HelpExpected,
	"hide_possible_values":            (*Command).HidePossibleValues,
	"infer_long_args":                 (*Command).InferLongArgs,
	"infer_subcommands":               (*Command).InferSubcommands,
	"arg_required_else_help":          (*Command).ArgRequiredElseHelp,
	"allow_missing_positional":        (*Command).AllowMissingPositional,
	"hide":                            (*Command).Hide,
	"subcommand_required":             (*Command).SubcommandRequired,
	"allow_external_subcommands":      (*Command).AllowExternalSubcommands,
	"args_conflicts_with_subcommands": (*Command).ArgsConflictsWithSubcommands,
	"subcommand_precedence_over_arg":  (*Command).SubcommandPrecedenceOverArg,
	"subcommand_negates_reqs":         (*Command).SubcommandNegatesReqs,
	"flatten_help":                    (*Command).FlattenHelp,
	"multicall":                       (*Command).Multicall,
}

func specApplyCommandCall(cmd *Command, call specParams) (*Command, error) {
	if setter, ok := specCommandStringSetters[call.method]; ok {
		s, err := call.str(0)
		if err != nil {
			return nil, err
		}
		return setter(cmd, s), nil
	}
	if setter, ok := specCommandBoolSetters[call.method]; ok {
		b, err := call.boolean(0)
		if err != nil {
			return nil, err
		}
		return setter(cmd, b), nil
	}
	switch call.method {
	case "aliases", "visible_aliases":
		names, err := call.strs(0)
		if err != nil {
			return nil, err
		}
		if call.method == "aliases" {
			return cmd.Aliases(names...), nil
		}
		return cmd.VisibleAliases(names...), nil
	case "short_flag", "short_flag_alias", "visible_short_flag_alias":
		c, err := call.char(0)
		if err != nil {
			return nil, err
		}
		switch call.method {
		case "short_flag":
			return cmd.ShortFlag(c), nil
		case "short_flag_alias":
			return cmd.ShortFlagAlias(c), nil
		default:
			return cmd.VisibleShortFlagAlias(c), nil
		}
	case "next_help_heading":
		s, err := call.optStr(0)
		if err != nil {
			return nil, err
		}
		if v, ok := s.Get(); ok {
			return cmd.NextHelpHeading(v), nil
		}
		return cmd.NextHelpHeadingReset(), nil
	case "next_display_order":
		n, err := call.optNum(0)
		if err != nil {
			return nil, err
		}
		if v, ok := n.Get(); ok {
			return cmd.NextDisplayOrder(v), nil
		}
		return cmd.NextDisplayOrderReset(), nil
	case "display_order", "term_width", "max_term_width":
		n, err := call.num(0)
		if err != nil {
			return nil, err
		}
		switch call.method {
		case "display_order":
			return cmd.DisplayOrder(n), nil
		case "term_width":
			return cmd.TermWidth(n), nil
		default:
			return cmd.MaxTermWidth(n), nil
		}
	case "color":
		s, err := call.str(0)
		if err != nil {
			return nil, err
		}
		color, err := ColorChoiceFromStr(s)
		if err != nil {
			return nil, err
		}
		return cmd.Color(color), nil
	case "styles":
		s, err := call.str(0)
		if err != nil {
			return nil, err
		}
		switch s {
		case "plain":
			return cmd.Styles(BuilderStylesPlain()), nil
		case "styled":
			return cmd.Styles(BuilderStylesStyled()), nil
		case "custom":
			return cmd.Styles(specCustomStyles()), nil
		}
		return nil, fmt.Errorf("unknown styles %q", s)
	case "external_subcommand_value_parser":
		vp, err := specValueParser(call.params[0])
		if err != nil {
			return nil, err
		}
		return cmd.ExternalSubcommandValueParser(vp), nil
	case "arg":
		arg, err := specBuildArg(call.params[0])
		if err != nil {
			return nil, err
		}
		return cmd.Arg(arg), nil
	case "group":
		group, err := specBuildGroup(call.params[0])
		if err != nil {
			return nil, err
		}
		return cmd.Group(group), nil
	case "subcommand":
		sub, err := specBuildCommand(call.params[0])
		if err != nil {
			return nil, err
		}
		return cmd.Subcommand(sub), nil
	case "mut_arg":
		id, err := call.str(0)
		if err != nil {
			return nil, err
		}
		var calls []json.RawMessage
		if err := json.Unmarshal(call.params[1], &calls); err != nil {
			return nil, err
		}
		var applyErr error
		cmd = cmd.MutArg(id, func(a *Arg) *Arg {
			a, applyErr = specApplyArgCalls(a, calls)
			return a
		})
		return cmd, applyErr
	case "mut_subcommand":
		name, err := call.str(0)
		if err != nil {
			return nil, err
		}
		var calls []json.RawMessage
		if err := json.Unmarshal(call.params[1], &calls); err != nil {
			return nil, err
		}
		var applyErr error
		cmd = cmd.MutSubcommand(name, func(c *Command) *Command {
			c, applyErr = specApplyCommandCalls(c, calls)
			return c
		})
		return cmd, applyErr
	}
	return nil, fmt.Errorf("unknown command method %q", call.method)
}

// specCustomStyles is the "custom" styles of the protocol.
func specCustomStyles() BuilderStyles {
	return BuilderStylesStyled().
		Header(BuilderStylingAnsiColor_Yellow.OnDefault().Bold().Underline()).
		Usage(BuilderStylingAnsiColor_Green.OnDefault().Bold()).
		Literal(BuilderStylingAnsiColor_Cyan.OnDefault().Bold()).
		Placeholder(BuilderStylingAnsiColor_Cyan.OnDefault()).
		Error(BuilderStylingAnsiColor_Red.OnDefault().Bold()).
		Valid(BuilderStylingAnsiColor_Green.OnDefault()).
		Invalid(BuilderStylingAnsiColor_Yellow.OnDefault().Bold()).
		Context(BuilderStylingRgbColor{F0: 10, F1: 20, F2: 30}.OnDefault().Dimmed()).
		ContextValue(BuilderStylingAnsi256Color(208).OnDefault().Italic())
}

func specBuildArg(raw json.RawMessage) (*Arg, error) {
	var spec specArg
	if err := json.Unmarshal(raw, &spec); err != nil {
		return nil, err
	}
	return specApplyArgCalls(ArgNew(spec.Id), spec.Calls)
}

func specApplyArgCalls(arg *Arg, calls []json.RawMessage) (*Arg, error) {
	for _, raw := range calls {
		call, err := decodeCall(raw)
		if err != nil {
			return nil, err
		}
		arg, err = specApplyArgCall(arg, call)
		if err != nil {
			return nil, err
		}
	}
	return arg, nil
}

var specArgStringSetters = map[string]func(*Arg, string) *Arg{
	"long":                    (*Arg).Long,
	"alias":                   (*Arg).Alias,
	"visible_alias":           (*Arg).VisibleAlias,
	"value_name":              (*Arg).ValueName,
	"value_terminator":        (*Arg).ValueTerminator,
	"default_value":           (*Arg).DefaultValue,
	"default_missing_value":   (*Arg).DefaultMissingValue,
	"env":                     (*Arg).Env,
	"help":                    (*Arg).Help,
	"long_help":               (*Arg).LongHelp,
	"requires":                (*Arg).Requires,
	"group":                   (*Arg).Group,
	"required_unless_present": (*Arg).RequiredUnlessPresent,
	"conflicts_with":          (*Arg).ConflictsWith,
	"overrides_with":          (*Arg).OverridesWith,
}

var specArgStringsSetters = map[string]func(*Arg, ...string) *Arg{
	"aliases":                     (*Arg).Aliases,
	"visible_aliases":             (*Arg).VisibleAliases,
	"value_names":                 (*Arg).ValueNames,
	"default_values":              (*Arg).DefaultValues,
	"default_missing_values":      (*Arg).DefaultMissingValues,
	"groups":                      (*Arg).Groups,
	"required_unless_present_all": (*Arg).RequiredUnlessPresentAll,
	"required_unless_present_any": (*Arg).RequiredUnlessPresentAny,
	"conflicts_with_all":          (*Arg).ConflictsWithAll,
	"overrides_with_all":          (*Arg).OverridesWithAll,
}

var specArgBoolSetters = map[string]func(*Arg, bool) *Arg{
	"trailing_var_arg":       (*Arg).TrailingVarArg,
	"last":                   (*Arg).Last,
	"required":               (*Arg).Required,
	"exclusive":              (*Arg).Exclusive,
	"global":                 (*Arg).Global,
	"ignore_case":            (*Arg).IgnoreCase,
	"allow_hyphen_values":    (*Arg).AllowHyphenValues,
	"allow_negative_numbers": (*Arg).AllowNegativeNumbers,
	"require_equals":         (*Arg).RequireEquals,
	"raw":                    (*Arg).Raw,
	"next_line_help":         (*Arg).NextLineHelp,
	"hide":                   (*Arg).Hide,
	"hide_possible_values":   (*Arg).HidePossibleValues,
	"hide_default_value":     (*Arg).HideDefaultValue,
	"hide_env":               (*Arg).HideEnv,
	"hide_env_values":        (*Arg).HideEnvValues,
	"hide_short_help":        (*Arg).HideShortHelp,
	"hide_long_help":         (*Arg).HideLongHelp,
}

var specActions = map[string]ArgAction{
	"set":        ArgAction_Set,
	"append":     ArgAction_Append,
	"set_true":   ArgAction_SetTrue,
	"set_false":  ArgAction_SetFalse,
	"count":      ArgAction_Count,
	"help":       ArgAction_Help,
	"help_short": ArgAction_HelpShort,
	"help_long":  ArgAction_HelpLong,
	"version":    ArgAction_Version,
}

func specApplyArgCall(arg *Arg, call specParams) (*Arg, error) {
	if setter, ok := specArgStringSetters[call.method]; ok {
		s, err := call.str(0)
		if err != nil {
			return nil, err
		}
		return setter(arg, s), nil
	}
	if setter, ok := specArgStringsSetters[call.method]; ok {
		s, err := call.strs(0)
		if err != nil {
			return nil, err
		}
		return setter(arg, s...), nil
	}
	if setter, ok := specArgBoolSetters[call.method]; ok {
		b, err := call.boolean(0)
		if err != nil {
			return nil, err
		}
		return setter(arg, b), nil
	}
	switch call.method {
	case "short", "short_alias", "visible_short_alias", "value_delimiter":
		c, err := call.char(0)
		if err != nil {
			return nil, err
		}
		switch call.method {
		case "short":
			return arg.Short(c), nil
		case "short_alias":
			return arg.ShortAlias(c), nil
		case "visible_short_alias":
			return arg.VisibleShortAlias(c), nil
		default:
			return arg.ValueDelimiter(c), nil
		}
	case "index", "display_order":
		n, err := call.num(0)
		if err != nil {
			return nil, err
		}
		if call.method == "index" {
			return arg.Index(n), nil
		}
		return arg.DisplayOrder(n), nil
	case "help_heading":
		s, err := call.optStr(0)
		if err != nil {
			return nil, err
		}
		if v, ok := s.Get(); ok {
			return arg.HelpHeading(v), nil
		}
		return arg.HelpHeadingReset(), nil
	case "required_if_eq":
		id, err := call.str(0)
		if err != nil {
			return nil, err
		}
		val, err := call.str(1)
		if err != nil {
			return nil, err
		}
		return arg.RequiredIfEq(id, val), nil
	case "required_if_eq_any", "required_if_eq_all":
		var pairs [][2]string
		if err := json.Unmarshal(call.params[0], &pairs); err != nil {
			return nil, err
		}
		ifs := make([]rs.Tuple2[string, string], len(pairs))
		for i, pair := range pairs {
			ifs[i] = rs.Tuple2[string, string]{F0: pair[0], F1: pair[1]}
		}
		if call.method == "required_if_eq_any" {
			return arg.RequiredIfEqAny(ifs...), nil
		}
		return arg.RequiredIfEqAll(ifs...), nil
	case "action":
		s, err := call.str(0)
		if err != nil {
			return nil, err
		}
		action, ok := specActions[s]
		if !ok {
			return nil, fmt.Errorf("unknown action %q", s)
		}
		return arg.Action(action), nil
	case "num_args":
		var n int
		if err := json.Unmarshal(call.params[0], &n); err == nil {
			return arg.NumArgs(BuilderValueRangeFromUsize(n)), nil
		}
		var bounds [2]*int
		if err := json.Unmarshal(call.params[0], &bounds); err != nil || bounds[0] == nil {
			return nil, fmt.Errorf("num_args: bad range %s", call.params[0])
		}
		if bounds[1] == nil {
			return arg.NumArgs(BuilderValueRangeFromRangeFrom(*bounds[0])), nil
		}
		return arg.NumArgs(BuilderValueRangeFromRangeInclusive(*bounds[0], *bounds[1])), nil
	case "value_parser":
		vp, err := specValueParser(call.params[0])
		if err != nil {
			return nil, err
		}
		return arg.ValueParser(vp), nil
	case "default_value_if":
		id, err := call.str(0)
		if err != nil {
			return nil, err
		}
		pred, err := specPredicate(call.params[1])
		if err != nil {
			return nil, err
		}
		def, err := call.optStr(2)
		if err != nil {
			return nil, err
		}
		return arg.DefaultValueIf(id, pred, def), nil
	case "requires_if":
		val, err := call.str(0)
		if err != nil {
			return nil, err
		}
		id, err := call.str(1)
		if err != nil {
			return nil, err
		}
		return arg.RequiresIf(BuilderArgPredicateFromStr(val), id), nil
	}
	return nil, fmt.Errorf("unknown arg method %q", call.method)
}

func specPredicate(raw json.RawMessage) (BuilderArgPredicate, error) {
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		if s == "present" {
			return &BuilderArgPredicate_IsPresent{}, nil
		}
		return nil, fmt.Errorf("unknown predicate %q", s)
	}
	var eq struct {
		Equals *string `json:"equals"`
	}
	if err := json.Unmarshal(raw, &eq); err != nil || eq.Equals == nil {
		return nil, fmt.Errorf("bad predicate %s", raw)
	}
	return BuilderArgPredicateFromStr(*eq.Equals), nil
}

// specEven is the protocol's "fn_even" value parser.
func specEven(s string) (int64, error) {
	n, err := parseInt(s, 64)
	if err != nil {
		return 0, err
	}
	if n%2 == 0 {
		return n, nil
	}
	return 0, stringError(strconv.FormatInt(n, 10) + " is odd")
}

// specUpper is the protocol's "fn_upper" value parser.
func specUpper(s string) (string, error) {
	if s == "" {
		return "", stringError("empty value")
	}
	return strToAsciiUppercase(s), nil
}

func strToAsciiUppercase(s string) string {
	b := []byte(s)
	for i, c := range b {
		if 'a' <= c && c <= 'z' {
			b[i] = c - ('a' - 'A')
		}
	}
	return string(b)
}

func specValueParser(raw json.RawMessage) (BuilderValueParser, error) {
	var name string
	if err := json.Unmarshal(raw, &name); err == nil {
		switch name {
		case "string":
			return ValueParserString(), nil
		case "os_string":
			return ValueParserOsString(), nil
		case "path":
			return ValueParserPathBuf(), nil
		case "bool":
			return ValueParserBool(), nil
		case "boolish":
			return BuilderBoolishValueParserNew().IntoValueParser(), nil
		case "falsey":
			return BuilderFalseyValueParserNew().IntoValueParser(), nil
		case "non_empty_string":
			return BuilderNonEmptyStringValueParserNew().IntoValueParser(), nil
		case "u8":
			return ValueParserU8().IntoValueParser(), nil
		case "i8":
			return ValueParserI8().IntoValueParser(), nil
		case "u16":
			return ValueParserU16().IntoValueParser(), nil
		case "i16":
			return ValueParserI16().IntoValueParser(), nil
		case "u32":
			return ValueParserU32().IntoValueParser(), nil
		case "i32":
			return ValueParserI32().IntoValueParser(), nil
		case "u64":
			return ValueParserU64().IntoValueParser(), nil
		case "i64":
			return ValueParserI64().IntoValueParser(), nil
		case "usize":
			return ValueParserUsize(), nil
		case "isize":
			return ValueParserIsize(), nil
		case "f64":
			return ValueParserF64(), nil
		case "f32":
			return ValueParserF32(), nil
		case "color":
			return ValueParserEnum[ColorChoice]().IntoValueParser(), nil
		case "mode":
			return ValueParserEnum[testMode]().IntoValueParser(), nil
		case "fn_even":
			return BuilderValueParserFromFn(specEven), nil
		case "fn_upper":
			return BuilderValueParserFromFn(specUpper), nil
		}
		return BuilderValueParser{}, fmt.Errorf("unknown value parser %q", name)
	}
	var obj struct {
		Possible       []json.RawMessage `json:"possible"`
		I64Range       *[2]*int64        `json:"i64_range"`
		U64Range       *[2]*uint64       `json:"u64_range"`
		UnknownSuggest *string           `json:"unknown_suggest"`
	}
	if err := json.Unmarshal(raw, &obj); err != nil {
		return BuilderValueParser{}, err
	}
	switch {
	case obj.Possible != nil:
		var values []BuilderPossibleValue
		for _, pvRaw := range obj.Possible {
			var name string
			if err := json.Unmarshal(pvRaw, &name); err == nil {
				values = append(values, BuilderPossibleValueNew(name))
				continue
			}
			var pv struct {
				Name    string   `json:"name"`
				Help    *string  `json:"help"`
				Hide    *bool    `json:"hide"`
				Aliases []string `json:"aliases"`
			}
			if err := json.Unmarshal(pvRaw, &pv); err != nil {
				return BuilderValueParser{}, err
			}
			value := BuilderPossibleValueNew(pv.Name)
			if pv.Help != nil {
				value = value.Help(*pv.Help)
			}
			if pv.Hide != nil {
				value = value.Hide(*pv.Hide)
			}
			if pv.Aliases != nil {
				value = value.Aliases(pv.Aliases...)
			}
			values = append(values, value)
		}
		return BuilderPossibleValuesParserFrom(values...).IntoValueParser(), nil
	case obj.I64Range != nil:
		bounds := RangeFull[int64]()
		lo, hi := obj.I64Range[0], obj.I64Range[1]
		switch {
		case lo != nil && hi != nil:
			bounds = RangeInclusive(*lo, *hi)
		case lo != nil:
			bounds = RangeFrom(*lo)
		case hi != nil:
			bounds = RangeToInclusive(*hi)
		}
		return ValueParserI64().Range(bounds).IntoValueParser(), nil
	case obj.U64Range != nil:
		bounds := RangeFull[uint64]()
		lo, hi := obj.U64Range[0], obj.U64Range[1]
		switch {
		case lo != nil && hi != nil:
			bounds = RangeInclusive(*lo, *hi)
		case lo != nil:
			bounds = RangeFrom(*lo)
		case hi != nil:
			bounds = RangeToInclusive(*hi)
		}
		return ValueParserU64().Range(bounds).IntoValueParser(), nil
	case obj.UnknownSuggest != nil:
		return BuilderUnknownArgumentValueParserSuggestArg(*obj.UnknownSuggest).IntoValueParser(), nil
	}
	return BuilderValueParser{}, fmt.Errorf("unknown value parser %s", raw)
}

func specBuildGroup(raw json.RawMessage) (*ArgGroup, error) {
	var spec specGroup
	if err := json.Unmarshal(raw, &spec); err != nil {
		return nil, err
	}
	group := ArgGroupNew(spec.Id)
	for _, callRaw := range spec.Calls {
		call, err := decodeCall(callRaw)
		if err != nil {
			return nil, err
		}
		switch call.method {
		case "arg", "requires", "conflicts_with":
			s, err := call.str(0)
			if err != nil {
				return nil, err
			}
			switch call.method {
			case "arg":
				group = group.Arg(s)
			case "requires":
				group = group.Requires(s)
			default:
				group = group.ConflictsWith(s)
			}
		case "args", "requires_all", "conflicts_with_all":
			s, err := call.strs(0)
			if err != nil {
				return nil, err
			}
			switch call.method {
			case "args":
				group = group.Args(s...)
			case "requires_all":
				group = group.RequiresAll(s...)
			default:
				group = group.ConflictsWithAll(s...)
			}
		case "required", "multiple":
			b, err := call.boolean(0)
			if err != nil {
				return nil, err
			}
			if call.method == "required" {
				group = group.Required(b)
			} else {
				group = group.Multiple(b)
			}
		default:
			return nil, fmt.Errorf("unknown group method %q", call.method)
		}
	}
	return group, nil
}

// testMode is the protocol's test enum, as `#[derive(ValueEnum)]` makes it:
//
//	enum Mode {
//	    /// Go quickly
//	    Fast,
//	    #[value(alias = "sl", help = "Take your time")]
//	    Slow,
//	    #[value(name = "very-careful")]
//	    VeryCareful,
//	    #[value(skip)]
//	    Unused,
//	    #[value(hide = true)]
//	    Secret,
//	}
type testMode uint8

const (
	testMode_Fast testMode = iota
	testMode_Slow
	testMode_VeryCareful
	testMode_Unused
	testMode_Secret
)

func (testMode) ValueVariants() []testMode {
	return []testMode{testMode_Fast, testMode_Slow, testMode_VeryCareful, testMode_Secret}
}

func (m testMode) ToPossibleValue() rs.Option[BuilderPossibleValue] {
	switch m {
	case testMode_Fast:
		return rs.Some(BuilderPossibleValueNew("fast").Help("Go quickly"))
	case testMode_Slow:
		return rs.Some(BuilderPossibleValueNew("slow").Alias("sl").Help("Take your time"))
	case testMode_VeryCareful:
		return rs.Some(BuilderPossibleValueNew("very-careful"))
	case testMode_Unused:
		return rs.None[BuilderPossibleValue]()
	case testMode_Secret:
		return rs.Some(BuilderPossibleValueNew("secret").Hide(true))
	default:
		panic(rs.Unreachable(m))
	}
}

// withEnv sets the request's environment variables, runs f, and removes
// them again.
func withEnv(env map[string]string, f func() any) any {
	for name, value := range env {
		os.Setenv(name, value)
	}
	defer func() {
		for name := range env {
			os.Unsetenv(name)
		}
	}()
	return f()
}

// dumpError is the protocol's ERR.
func dumpError(err error) map[string]any {
	e := err.(*Error)
	rendered := e.Render()
	context := []any{}
	for _, entry := range e.Context() {
		context = append(context, []any{entry.Kind.debugString(), entry.Value.String()})
	}
	return map[string]any{
		"kind":    e.Kind().debugString(),
		"stderr":  e.UseStderr(),
		"code":    e.ExitCode(),
		"plain":   rendered.String(),
		"ansi":    rendered.Ansi(),
		"context": context,
	}
}

// dumpTyped tries the protocol's types in order on an argument's values.
func dumpTyped(m *ArgMatches, id string) (any, any) {
	attempts := []struct {
		tag string
		get func() (rs.Option[[][]any], error)
	}{
		{"str", typedOccurrences[string](m, id, func(v string) any { return v })},
		{"bool", typedOccurrences[bool](m, id, func(v bool) any { return v })},
		{"u8", typedOccurrences[uint8](m, id, func(v uint8) any { return v })},
		{"i8", typedOccurrences[int8](m, id, func(v int8) any { return v })},
		{"u16", typedOccurrences[uint16](m, id, func(v uint16) any { return v })},
		{"i16", typedOccurrences[int16](m, id, func(v int16) any { return v })},
		{"u32", typedOccurrences[uint32](m, id, func(v uint32) any { return v })},
		{"i32", typedOccurrences[int32](m, id, func(v int32) any { return v })},
		{"u64", typedOccurrences[uint64](m, id, func(v uint64) any { return v })},
		{"i64", typedOccurrences[int64](m, id, func(v int64) any { return v })},
		{"size", typedOccurrences[int](m, id, func(v int) any { return v })},
		{"f64", typedOccurrences[float64](m, id, func(v float64) any { return rustFloatDisplay(v, 64) })},
		{"f32", typedOccurrences[float32](m, id, func(v float32) any { return rustFloatDisplay(float64(v), 32) })},
		{"id", typedOccurrences[Id](m, id, func(v Id) any { return string(v) })},
		{"color", typedOccurrences[ColorChoice](m, id, func(v ColorChoice) any { return v.String() })},
		{"mode", typedOccurrences[testMode](m, id, func(v testMode) any {
			pv := v.ToPossibleValue().Unwrap()
			return pv.GetName()
		})},
	}
	for _, a := range attempts {
		occs, err := a.get()
		if err != nil {
			continue
		}
		got, ok := occs.Get()
		if !ok {
			return nil, nil
		}
		return a.tag, got
	}
	return nil, nil
}

func typedOccurrences[T any](m *ArgMatches, id string, convert func(T) any) func() (rs.Option[[][]any], error) {
	return func() (rs.Option[[][]any], error) {
		occs, err := ArgMatchesTryGetOccurrences[T](m, id)
		if err != nil {
			return rs.None[[][]any](), err
		}
		got, ok := occs.Get()
		if !ok {
			return rs.None[[][]any](), nil
		}
		out := make([][]any, len(got))
		for i, occ := range got {
			out[i] = make([]any, len(occ))
			for j, v := range occ {
				out[i][j] = convert(v)
			}
		}
		return rs.Some(out), nil
	}
}

// rustFloatDisplay is `format!("{v}")` for an f64 or f32.
func rustFloatDisplay(v float64, bits int) string {
	switch {
	case math.IsInf(v, 1):
		return "inf"
	case math.IsInf(v, -1):
		return "-inf"
	case math.IsNaN(v):
		return "NaN"
	}
	return strconv.FormatFloat(v, 'f', -1, bits)
}

// dumpMatches is the protocol's MATCHES.
func dumpMatches(m *ArgMatches) map[string]any {
	ids := []any{}
	for _, id := range m.Ids() {
		name := id.AsStr()
		entry := map[string]any{"id": name}
		if source, ok := m.ValueSource(name).Get(); ok {
			entry["source"] = source.debugString()
		} else {
			entry["source"] = nil
		}
		if raw, ok := m.GetRawOccurrences(name).Get(); ok {
			entry["raw"] = raw
		} else {
			entry["raw"] = nil
		}
		if indices, ok := m.IndicesOf(name).Get(); ok {
			if indices == nil {
				indices = []int{}
			}
			entry["idx"] = indices
		} else {
			entry["idx"] = nil
		}
		tag, values := dumpTyped(m, name)
		entry["t"] = tag
		entry["v"] = values
		ids = append(ids, entry)
	}
	var sub any
	if name, subMatches, ok := subcommandOf(m); ok {
		sub = map[string]any{"name": name, "m": dumpMatches(subMatches)}
	}
	return map[string]any{"ids": ids, "sub": sub}
}

func subcommandOf(m *ArgMatches) (string, *ArgMatches, bool) {
	sc, ok := m.Subcommand().Get()
	if !ok {
		return "", nil, false
	}
	return sc.F0, sc.F1, true
}

// specRequest is every request field the clap module reads.
type specRequest struct {
	Op    string            `json:"op"`
	Cmd   json.RawMessage   `json:"cmd"`
	Argv  []string          `json:"argv"`
	Argvs [][]string        `json:"argvs"`
	Env   map[string]string `json:"env"`
	Path  []string          `json:"path"`
	What  string            `json:"what"`
}

// specRun is op "run" for one argv on a fresh command.
func specRun(cmdRaw json.RawMessage, argv []string) any {
	cmd, err := specBuildCommand(cmdRaw)
	if err != nil {
		return map[string]any{"oracle_error": err.Error()}
	}
	matches, err := cmd.TryGetMatchesFromMut(argv)
	if err != nil {
		return map[string]any{"err": dumpError(err)}
	}
	return map[string]any{"ok": dumpMatches(matches)}
}

// specRender is op "render" on a command.
func specRender(cmd *Command, path []string, what string) any {
	c := cmd
	if len(path) != 0 {
		cmd.Build()
		for _, name := range path {
			found, ok := c.FindSubcommandMut(name).Get()
			if !ok {
				return map[string]any{"oracle_error": "no subcommand " + name}
			}
			c = found
		}
	}
	styled := func(s BuilderStyledStr) any {
		return map[string]any{"plain": s.String(), "ansi": s.Ansi()}
	}
	switch what {
	case "help":
		return styled(c.RenderHelp())
	case "long_help":
		return styled(c.RenderLongHelp())
	case "usage":
		return styled(c.RenderUsage())
	case "version":
		return map[string]any{"plain": c.RenderVersion()}
	case "long_version":
		return map[string]any{"plain": c.RenderLongVersion()}
	}
	return map[string]any{"oracle_error": "unknown render " + what}
}

// answerSpec is the Go side of the generic ops.
func answerSpec(raw json.RawMessage) any {
	var req specRequest
	if err := json.Unmarshal(raw, &req); err != nil {
		return map[string]any{"oracle_error": err.Error()}
	}
	return withEnv(req.Env, func() any {
		return catchPanic(func() any {
			switch req.Op {
			case "run":
				return specRun(req.Cmd, req.Argv)
			case "runs":
				results := []any{}
				for _, argv := range req.Argvs {
					reply := specRun(req.Cmd, argv)
					if m, ok := reply.(map[string]any); ok && m["oracle_error"] != nil {
						return reply
					}
					results = append(results, reply)
				}
				return map[string]any{"results": results}
			case "render":
				cmd, err := specBuildCommand(req.Cmd)
				if err != nil {
					return map[string]any{"oracle_error": err.Error()}
				}
				return specRender(cmd, req.Path, req.What)
			}
			return map[string]any{"oracle_error": "unknown op " + req.Op}
		})
	})
}

// catchPanic is oracle.Catch without the dependency, so that this file
// stands alone: a panic is the reply {"panic": message}.
func catchPanic(f func() any) (reply any) {
	defer func() {
		if recovered := recover(); recovered != nil {
			reply = map[string]string{"panic": rs.PanicInfoOf(recovered).Message}
		}
	}()
	return f()
}
