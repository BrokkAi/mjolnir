// Port of clap_builder 4.6.0 src/builder/arg_group.rs

package clap

// ArgGroup is clap::ArgGroup: specifies a logical group of arguments.
//
// You can use this for
//   - applying validation to an entire group, like ArgGroup::multiple
//   - validate relationships between an argument and a group, like
//     conflicts or requirements
//   - check which argument in a group was specified on the command-line
//
// For visually grouping arguments in help, see instead Arg::help_heading.
//
// The builder methods change the group and return it, so calls chain as they
// do in Rust.
type ArgGroup struct {
	id        Id
	args      []Id
	required  bool
	requires  []Id
	conflicts []Id
	multiple  bool
}

// ArgGroupNew is ArgGroup::new: create an ArgGroup using a unique name.
//
// The name will be used to get values from the group or refer to the group
// inside of conflict and requirement rules.
func ArgGroupNew(id string) *ArgGroup {
	return (&ArgGroup{}).Id(id)
}

// Id is ArgGroup::id: sets the group name.
func (g *ArgGroup) Id(id string) *ArgGroup {
	g.id = Id(id)
	return g
}

// Arg is ArgGroup::arg: adds an argument to this group by name.
func (g *ArgGroup) Arg(argId string) *ArgGroup {
	g.args = append(g.args, Id(argId))
	return g
}

// ArgReset is ArgGroup::arg(None): removes the arguments.
func (g *ArgGroup) ArgReset() *ArgGroup {
	g.args = nil
	return g
}

// Args is ArgGroup::args: adds multiple arguments to this group by name.
func (g *ArgGroup) Args(ns ...string) *ArgGroup {
	for _, n := range ns {
		g = g.Arg(n)
	}
	return g
}

// GetArgs is ArgGroup::get_args: getters for all args. It will return a
// vector of Id.
func (g *ArgGroup) GetArgs() []Id {
	return g.args
}

// Multiple is ArgGroup::multiple: allows more than one of the Args in this
// group to be used. (Default: `false`)
func (g *ArgGroup) Multiple(yes bool) *ArgGroup {
	g.multiple = yes
	return g
}

// IsMultiple is ArgGroup::is_multiple: return true if the group allows more
// than one of the arguments in this group to be used. (Default: `false`)
func (g *ArgGroup) IsMultiple() bool {
	return g.multiple
}

// Required is ArgGroup::required: require an argument from the group to be
// present when parsing.
//
// This is unless conflicting with another argument. A required group will
// be displayed in the usage string of the application in the format
// `<arg|arg2|arg3>`.
//
// NOTE: This setting only applies to the current Command / Subcommands, and
// not globally.
//
// NOTE: By default, ArgGroup::multiple is set to `false` which when
// combined with `ArgGroup::required(true)` states, "One and *only one* arg
// must be used from this group. Use of more than one arg is an error."
// Vice setting `ArgGroup::multiple(true)` which states, '*At least* one arg
// from this group must be used. Using multiple is OK."
func (g *ArgGroup) Required(yes bool) *ArgGroup {
	g.required = yes
	return g
}

// Requires is ArgGroup::requires: specify an argument or group that must be
// present when this group is.
//
// This is not to be confused with a required group. Requirement rules
// function just like argument requirement rules, you can name other
// arguments or groups that must be present when any one of the arguments
// from this group is used.
//
// NOTE: The name provided may be an argument or group name
func (g *ArgGroup) Requires(id string) *ArgGroup {
	g.requires = append(g.requires, Id(id))
	return g
}

// RequiresReset is ArgGroup::requires(None).
func (g *ArgGroup) RequiresReset() *ArgGroup {
	g.requires = nil
	return g
}

// RequiresAll is ArgGroup::requires_all: specify arguments or groups that
// must be present when this group is.
func (g *ArgGroup) RequiresAll(ns ...string) *ArgGroup {
	for _, n := range ns {
		g = g.Requires(n)
	}
	return g
}

// ConflictsWith is ArgGroup::conflicts_with: specify an argument or group
// that must not be present when this group is.
//
// Exclusion (aka conflict) rules function just like argument exclusion
// rules, you can name other arguments or groups that must not be present
// when one of the arguments from this group are used.
//
// NOTE: The name provided may be an argument, or group name
func (g *ArgGroup) ConflictsWith(id string) *ArgGroup {
	g.conflicts = append(g.conflicts, Id(id))
	return g
}

// ConflictsWithReset is ArgGroup::conflicts_with(None).
func (g *ArgGroup) ConflictsWithReset() *ArgGroup {
	g.conflicts = nil
	return g
}

// ConflictsWithAll is ArgGroup::conflicts_with_all: specify arguments or
// groups that must not be present when this group is.
func (g *ArgGroup) ConflictsWithAll(ns ...string) *ArgGroup {
	for _, n := range ns {
		g = g.ConflictsWith(n)
	}
	return g
}

// GetId is ArgGroup::get_id: get the name of the group.
func (g *ArgGroup) GetId() Id {
	return g.id
}

// IsRequiredSet is ArgGroup::is_required_set: reports whether
// ArgGroup::required is set.
func (g *ArgGroup) IsRequiredSet() bool {
	return g.required
}

// Clone is `impl Clone for ArgGroup`.
func (g *ArgGroup) Clone() *ArgGroup {
	return &ArgGroup{
		id:        g.id,
		args:      cloneIds(g.args),
		required:  g.required,
		requires:  cloneIds(g.requires),
		conflicts: cloneIds(g.conflicts),
		multiple:  g.multiple,
	}
}
