// Port of clap_builder 4.6.0 src/output/usage.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

const outputUsageDefaultSubValueName = "COMMAND"
const outputUsageSep = "\n       "

// outputUsage is Usage.
type outputUsage struct {
	cmd      *Command
	styles   *BuilderStyles
	required *utilChildGraph
}

func outputUsageNew(cmd *Command) *outputUsage {
	return &outputUsage{
		cmd:      cmd,
		styles:   cmd.GetStyles(),
		required: nil,
	}
}

func (u *outputUsage) withRequired(required *utilChildGraph) *outputUsage {
	u.required = required
	return u
}

// Creates a usage string for display. This happens just after all arguments were parsed, but before
// any subcommands have been parsed (so as to give subcommands their own usage recursively)
func (u *outputUsage) createUsageWithTitle(used []Id) rs.Option[BuilderStyledStr] {
	styled := BuilderStyledStrNew()
	styled.PushStr(u.styles.GetUsage().Render() + "Usage:" + u.styles.GetUsage().RenderReset() + " ")
	if u.writeUsageNoTitle(&styled, used) {
		styled.trimEnd()
	} else {
		return rs.None[BuilderStyledStr]()
	}
	return rs.Some(styled)
}

// Creates a usage string (*without title*) if one was not provided by the user manually.
func (u *outputUsage) createUsageNoTitle(used []Id) rs.Option[BuilderStyledStr] {
	styled := BuilderStyledStrNew()
	if u.writeUsageNoTitle(&styled, used) {
		styled.trimEnd()
		return rs.Some(styled)
	}
	return rs.None[BuilderStyledStr]()
}

// Creates a usage string (*without title*) if one was not provided by the user manually.
func (u *outputUsage) writeUsageNoTitle(styled *BuilderStyledStr, used []Id) bool {
	if usage, ok := u.cmd.getOverrideUsage().Get(); ok {
		styled.pushStyled(usage)
		return true
	}
	if len(used) == 0 {
		u.writeHelpUsage(styled)
	} else {
		u.writeSmartUsage(styled, used)
	}
	return true
}

// Creates a usage string for display in help messages (i.e. not for errors)
func (u *outputUsage) writeHelpUsage(styled *BuilderStyledStr) {
	if u.cmd.hasVisibleSubcommands() && u.cmd.IsFlattenHelpSet() {
		if !u.cmd.IsSubcommandRequiredSet() ||
			u.cmd.IsArgsConflictsWithSubcommandsSet() {
			u.writeArgUsage(styled, nil, true)
			styled.trimEnd()
			styled.PushStr(outputUsageSep)
		}
		cmd := u.cmd.Clone()
		cmd.Build()

		i := 0
		for _, sub := range cmd.GetSubcommands() {
			if sub.IsHideSet() {
				continue
			}
			if i != 0 {
				styled.trimEnd()
				styled.PushStr(outputUsageSep)
			}
			outputUsageNew(sub).writeUsageNoTitle(styled, nil)
			i++
		}
	} else {
		u.writeArgUsage(styled, nil, true)
		u.writeSubcommandUsage(styled)
	}
}

// Creates a context aware usage string, or "smart usage" from currently used
// args, and requirements
func (u *outputUsage) writeSmartUsage(styled *BuilderStyledStr, used []Id) {
	placeholder := u.styles.GetPlaceholder()

	u.writeArgUsage(styled, used, true)

	if u.cmd.IsSubcommandRequiredSet() {
		valueName := u.cmd.GetSubcommandValueName().UnwrapOr(outputUsageDefaultSubValueName)
		styled.PushStr(placeholder.Render() + "<" + valueName + ">" + placeholder.RenderReset())
	}
}

func (u *outputUsage) writeArgUsage(styled *BuilderStyledStr, used []Id, inclReqs bool) {
	literal := u.styles.GetLiteral()
	placeholder := u.styles.GetPlaceholder()

	binName := u.cmd.getUsageNameFallback()
	if binName != "" {
		// the trim won't properly remove a leading space due to the formatting
		styled.PushStr(literal.Render() + binName + literal.RenderReset() + " ")
	}

	if len(used) == 0 && u.needsOptionsTag() {
		styled.PushStr(placeholder.Render() + "[OPTIONS]" + placeholder.RenderReset() + " ")
	}

	u.writeArgs(styled, used, !inclReqs)
}

func (u *outputUsage) writeSubcommandUsage(styled *BuilderStyledStr) {
	// incl_reqs is only false when this function is called recursively
	if u.cmd.hasVisibleSubcommands() || u.cmd.IsAllowExternalSubcommandsSet() {
		literal := u.styles.GetLiteral()
		placeholder := u.styles.GetPlaceholder()
		valueName := u.cmd.GetSubcommandValueName().UnwrapOr(outputUsageDefaultSubValueName)
		if u.cmd.IsSubcommandNegatesReqsSet() ||
			u.cmd.IsArgsConflictsWithSubcommandsSet() {
			styled.trimEnd()
			styled.PushStr(outputUsageSep)
			if u.cmd.IsArgsConflictsWithSubcommandsSet() {
				binName := u.cmd.getUsageNameFallback()
				// Short-circuit full usage creation since no args will be relevant
				styled.PushStr(literal.Render() + binName + literal.RenderReset() + " ")
			} else {
				u.writeArgUsage(styled, nil, false)
			}
			styled.PushStr(placeholder.Render() + "<" + valueName + ">" + placeholder.RenderReset())
		} else if u.cmd.IsSubcommandRequiredSet() {
			styled.PushStr(placeholder.Render() + "<" + valueName + ">" + placeholder.RenderReset())
		} else {
			styled.PushStr(placeholder.Render() + "[" + valueName + "]" + placeholder.RenderReset())
		}
	}
}

// Determines if we need the `[OPTIONS]` tag in the usage string
func (u *outputUsage) needsOptionsTag() bool {
outer:
	for _, f := range u.cmd.getNonPositionals() {
		// Don't print `[OPTIONS]` just for help or version
		if long, ok := f.GetLong().Get(); ok && (long == "help" || long == "version") {
			continue
		}
		switch f.GetAction() {
		case ArgAction_Set,
			ArgAction_Append,
			ArgAction_SetTrue,
			ArgAction_SetFalse,
			ArgAction_Count:
		case ArgAction_Help,
			ArgAction_HelpShort,
			ArgAction_HelpLong,
			ArgAction_Version:
			continue
		default:
			panic(rs.Unreachable(f.GetAction()))
		}

		if f.IsHideSet() {
			continue
		}
		if f.IsRequiredSet() {
			continue
		}
		for _, grpS := range u.cmd.groupsForArg(f.GetId()) {
			for _, g := range u.cmd.GetGroups() {
				if g.id == grpS && g.required {
					continue outer
				}
			}
		}

		return true
	}

	return false
}

// writeArgs returns the required args in usage string form by fully
// unrolling all groups
func (u *outputUsage) writeArgs(styled *BuilderStyledStr, incls []Id, forceOptional bool) {
	literal := u.styles.GetLiteral()

	required := u.required
	if required == nil {
		required = u.cmd.requiredGraph()
	}
	var unrolledReqs []Id
	for _, a := range required.iter() {
		isRelevant := func(r *argRequire) (Id, bool) {
			var required bool
			switch r.predicate.(type) {
			case *BuilderArgPredicate_Equals:
				required = false
			case *BuilderArgPredicate_IsPresent:
				required = true
			default:
				panic(rs.Unreachable(r.predicate))
			}
			return r.id, required
		}

		// if we don't check for duplicates here this causes duplicate error messages
		// see https://github.com/clap-rs/clap/issues/2770
		unrolledReqs = append(unrolledReqs, u.cmd.unrollArgRequires(isRelevant, a)...)
		// always include the required arg itself. it will not be enumerated
		// by unroll_requirements_for_arg.
		unrolledReqs = append(unrolledReqs, a)
	}

	reqs := append(append([]Id{}, unrolledReqs...), incls...)

	var requiredGroupsMembers utilFlatSet[Id]
	var requiredGroups utilFlatSet[BuilderStyledStr]
	for _, req := range reqs {
		if u.cmd.findGroup(req) != nil {
			groupMembers := u.cmd.unrollArgsInGroup(req)
			elem := u.cmd.formatGroup(req)
			requiredGroups.insert(elem)
			requiredGroupsMembers.extend(groupMembers)
		} else {
			if rs.DebugAssertions && u.cmd.find(req) == nil {
				rs.PanicAt(1, "assertion failed: self.cmd.find(req).is_some()")
			}
		}
	}

	var requiredOpts utilFlatSet[BuilderStyledStr]
	var requiredPositionals []rs.Option[BuilderStyledStr]
	for _, req := range reqs {
		if arg := u.cmd.find(req); arg != nil {
			if requiredGroupsMembers.contains(arg.GetId()) {
				continue
			}

			stylized := arg.stylized(u.styles, rs.Some(!forceOptional))
			if index, ok := arg.GetIndex().Get(); ok {
				newLen := index + 1
				for len(requiredPositionals) < newLen {
					requiredPositionals = append(requiredPositionals, rs.None[BuilderStyledStr]())
				}
				requiredPositionals[index] = rs.Some(stylized)
			} else {
				requiredOpts.insert(stylized)
			}
		} else {
			if rs.DebugAssertions && u.cmd.findGroup(req) == nil {
				rs.PanicAt(1, "assertion failed: self.cmd.find_group(req).is_some()")
			}
		}
	}

	for _, pos := range u.cmd.GetPositionals() {
		if pos.IsHideSet() {
			continue
		}
		if requiredGroupsMembers.contains(pos.GetId()) {
			continue
		}

		index := pos.GetIndex().Unwrap()
		newLen := index + 1
		for len(requiredPositionals) < newLen {
			requiredPositionals = append(requiredPositionals, rs.None[BuilderStyledStr]())
		}
		if requiredPositionals[index].IsSome() {
			if pos.IsLastSet() {
				styled := requiredPositionals[index].Take().Unwrap()
				newStyled := BuilderStyledStrNew()
				newStyled.PushStr(literal.Render() + "--" + literal.RenderReset() + " ")
				newStyled.pushStyled(styled)
				requiredPositionals[index] = rs.Some(newStyled)
			}
		} else {
			var styled BuilderStyledStr
			if pos.IsLastSet() {
				styled = BuilderStyledStrNew()
				styled.PushStr(literal.Render() + "[--" + literal.RenderReset() + " ")
				styled.pushStyled(pos.stylized(u.styles, rs.Some(true)))
				styled.PushStr(literal.Render() + "]" + literal.RenderReset())
			} else {
				styled = pos.stylized(u.styles, rs.Some(false))
			}
			requiredPositionals[index] = rs.Some(styled)
		}
		if pos.IsLastSet() && forceOptional {
			requiredPositionals[index] = rs.None[BuilderStyledStr]()
		}
	}

	if !forceOptional {
		for _, arg := range requiredOpts.intoVec() {
			styled.pushStyled(arg)
			styled.PushStr(" ")
		}
		for _, arg := range requiredGroups.intoVec() {
			styled.pushStyled(arg)
			styled.PushStr(" ")
		}
	}
	for _, arg := range requiredPositionals {
		if arg, ok := arg.Get(); ok {
			styled.pushStyled(arg)
			styled.PushStr(" ")
		}
	}
}

func (u *outputUsage) getRequiredUsageFrom(incls []Id, matcher *parserArgMatcher, inclLast bool) []BuilderStyledStr {
	required := u.required
	if required == nil {
		required = u.cmd.requiredGraph()
	}
	var unrolledReqs []Id
	for _, a := range required.iter() {
		isRelevant := func(r *argRequire) (Id, bool) {
			var required bool
			switch r.predicate.(type) {
			case *BuilderArgPredicate_Equals:
				if matcher != nil {
					required = matcher.checkExplicit(a, r.predicate)
				} else {
					required = false
				}
			case *BuilderArgPredicate_IsPresent:
				required = true
			default:
				panic(rs.Unreachable(r.predicate))
			}
			return r.id, required
		}

		// if we don't check for duplicates here this causes duplicate error messages
		// see https://github.com/clap-rs/clap/issues/2770
		unrolledReqs = append(unrolledReqs, u.cmd.unrollArgRequires(isRelevant, a)...)
		// always include the required arg itself. it will not be enumerated
		// by unroll_requirements_for_arg.
		unrolledReqs = append(unrolledReqs, a)
	}

	reqs := append(append([]Id{}, unrolledReqs...), incls...)

	var requiredGroupsMembers utilFlatSet[Id]
	var requiredGroups utilFlatSet[BuilderStyledStr]
	for _, req := range reqs {
		if u.cmd.findGroup(req) != nil {
			groupMembers := u.cmd.unrollArgsInGroup(req)
			isPresent := false
			if matcher != nil {
				for _, arg := range groupMembers {
					if matcher.checkExplicit(arg, argPredicateIsPresent) {
						isPresent = true
						break
					}
				}
			}
			if isPresent {
				continue
			}

			elem := u.cmd.formatGroup(req)
			requiredGroups.insert(elem)
			requiredGroupsMembers.extend(groupMembers)
		} else {
			if rs.DebugAssertions && u.cmd.find(req) == nil {
				rs.PanicAt(1, "`"+string(req)+"` must exist")
			}
		}
	}

	var requiredOpts utilFlatSet[BuilderStyledStr]
	var requiredPositionals []rs.Option[BuilderStyledStr]
	for _, req := range reqs {
		if arg := u.cmd.find(req); arg != nil {
			if requiredGroupsMembers.contains(arg.GetId()) {
				continue
			}

			isPresent := matcher != nil && matcher.checkExplicit(req, argPredicateIsPresent)
			if isPresent {
				continue
			}

			stylized := arg.stylized(u.styles, rs.Some(true))
			if index, ok := arg.GetIndex().Get(); ok {
				if !arg.IsLastSet() || inclLast {
					newLen := index + 1
					for len(requiredPositionals) < newLen {
						requiredPositionals = append(requiredPositionals, rs.None[BuilderStyledStr]())
					}
					requiredPositionals[index] = rs.Some(stylized)
				}
			} else {
				requiredOpts.insert(stylized)
			}
		} else {
			if rs.DebugAssertions && u.cmd.findGroup(req) == nil {
				rs.PanicAt(1, "assertion failed: self.cmd.find_group(req).is_some()")
			}
		}
	}

	var retVal []BuilderStyledStr
	retVal = append(retVal, requiredOpts.intoVec()...)
	retVal = append(retVal, requiredGroups.intoVec()...)
	for _, pos := range requiredPositionals {
		if pos, ok := pos.Get(); ok {
			retVal = append(retVal, pos)
		}
	}
	return retVal
}
