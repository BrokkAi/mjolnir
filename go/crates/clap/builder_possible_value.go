// Port of clap_builder 4.6.0 src/builder/possible_value.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// BuilderPossibleValue is clap::builder::PossibleValue: a possible value of
// an argument.
//
// A semantically equivalent alternative to using strings directly in
// PossibleValuesParser, but more information like a help message and
// aliases can be added.
//
// NOTE: Most likely you can use strings, rather than PossibleValue as it is
// only required to hide single values from help messages and shell
// completions or to attach help to possible values.
//
// A PossibleValue is a value: every setter returns a changed copy.
type BuilderPossibleValue struct {
	name    string
	help    rs.Option[BuilderStyledStr]
	aliases []string // (name, visible)
	hide    bool
}

// BuilderPossibleValueNew is PossibleValue::new: create a PossibleValue with
// its name.
//
// The name will be used to decide whether this value was provided by the
// user to an argument.
//
// NOTE: In case it is not hidden it will also be shown in help messages for
// arguments that use it as a possible value and have not hidden them through
// Arg::hide_possible_values(true).
func BuilderPossibleValueNew(name string) BuilderPossibleValue {
	return BuilderPossibleValue{name: name}
}

// Help is PossibleValue::help: sets the help description of the value.
//
// This is typically displayed in completions (where supported) and should be
// a short, one-line description.
func (p BuilderPossibleValue) Help(help string) BuilderPossibleValue {
	p.help = rs.Some(BuilderStyledStr(help))
	return p
}

// HelpReset is PossibleValue::help(None).
func (p BuilderPossibleValue) HelpReset() BuilderPossibleValue {
	p.help = rs.None[BuilderStyledStr]()
	return p
}

// Hide is PossibleValue::hide: hides this value from help and shell
// completions.
//
// This is an alternative to hiding through Arg::hide_possible_values(true),
// if you only want to hide some values.
func (p BuilderPossibleValue) Hide(yes bool) BuilderPossibleValue {
	p.hide = yes
	return p
}

// Alias is PossibleValue::alias: sets a hidden alias for this argument
// value.
func (p BuilderPossibleValue) Alias(name string) BuilderPossibleValue {
	p.aliases = append(append([]string(nil), p.aliases...), name)
	return p
}

// AliasReset is PossibleValue::alias(None): removes the aliases.
func (p BuilderPossibleValue) AliasReset() BuilderPossibleValue {
	p.aliases = nil
	return p
}

// Aliases is PossibleValue::aliases: sets multiple hidden aliases for this
// argument value.
func (p BuilderPossibleValue) Aliases(names ...string) BuilderPossibleValue {
	p.aliases = append(append([]string(nil), p.aliases...), names...)
	return p
}

// GetName is PossibleValue::get_name: get the name of the argument value.
func (p *BuilderPossibleValue) GetName() string { return p.name }

// GetHelp is PossibleValue::get_help: get the help specified for this
// argument, if any.
func (p *BuilderPossibleValue) GetHelp() rs.Option[BuilderStyledStr] { return p.help }

// IsHideSet is PossibleValue::is_hide_set: report if PossibleValue::hide is
// set.
func (p *BuilderPossibleValue) IsHideSet() bool { return p.hide }

// shouldShowHelp reports if PossibleValue is not hidden and has a help
// message.
func (p *BuilderPossibleValue) shouldShowHelp() bool {
	return !p.hide && p.help.IsSome()
}

// getVisibleQuotedName gets the name if argument value is not hidden, None
// otherwise, but wrapped in quotes if it contains whitespace.
func (p *BuilderPossibleValue) getVisibleQuotedName() rs.Option[string] {
	if !p.hide {
		return rs.Some(utilEscape(p.name))
	}
	return rs.None[string]()
}

// GetNameAndAliases is PossibleValue::get_name_and_aliases: returns all
// valid values of the argument value.
//
// Namely the name and all aliases.
func (p *BuilderPossibleValue) GetNameAndAliases() []string {
	names := make([]string, 0, 1+len(p.aliases))
	names = append(names, p.name)
	names = append(names, p.aliases...)
	return names
}

// Matches is PossibleValue::matches: tests if the value is valid for this
// argument value.
//
// The value is valid if it is either the name or one of the aliases.
func (p *BuilderPossibleValue) Matches(value string, ignoreCase bool) bool {
	for _, name := range p.GetNameAndAliases() {
		if ignoreCase {
			if utilEqIgnoreCase(name, value) {
				return true
			}
		} else if name == value {
			return true
		}
	}
	return false
}

// Equal is the derived PartialEq of PossibleValue.
func (p *BuilderPossibleValue) Equal(other *BuilderPossibleValue) bool {
	if p.name != other.name || p.hide != other.hide || len(p.aliases) != len(other.aliases) {
		return false
	}
	for i := range p.aliases {
		if p.aliases[i] != other.aliases[i] {
			return false
		}
	}
	a, aok := p.help.Get()
	b, bok := other.help.Get()
	return aok == bok && a == b
}

// BuilderPossibleValueFromStr is `impl<S: Into<Str>> From<S> for
// PossibleValue`.
func BuilderPossibleValueFromStr(s string) BuilderPossibleValue {
	return BuilderPossibleValueNew(s)
}
