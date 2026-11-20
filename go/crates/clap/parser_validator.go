// Port of clap_builder 4.6.0 src/parser/validator.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// parserValidator is Validator.
type parserValidator struct {
	cmd      *Command
	required *utilChildGraph
}

func parserValidatorNew(cmd *Command) *parserValidator {
	required := cmd.requiredGraph()
	return &parserValidator{cmd: cmd, required: required}
}

// matcherArgs is ArgMatcher::args: the matched arguments, in the order they
// were matched.
func matcherArgs(matcher *parserArgMatcher) ([]Id, []*parserMatchedArg) {
	return matcher.matches.args.keys, matcher.matches.args.values
}

func (v *parserValidator) validate(matcher *parserArgMatcher) *Error {
	conflicts := parserConflictsWithArgs(v.cmd, matcher)
	hasSubcmd := matcher.subcommandName().IsSome()

	if !hasSubcmd && v.cmd.IsArgRequiredElseHelpSet() {
		numUserValues := 0
		_, matched := matcherArgs(matcher)
		for _, m := range matched {
			if m.checkExplicit(argPredicateIsPresent) {
				numUserValues++
			}
		}
		if numUserValues == 0 {
			message := v.cmd.writeHelpErr(false)
			return errorDisplayHelpError(v.cmd, message)
		}
	}
	if !hasSubcmd && v.cmd.IsSubcommandRequiredSet() {
		bn := v.cmd.getBinNameFallback()
		return errorMissingSubcommand(v.cmd, bn, append([]string{}, v.cmd.allSubcommandNames()...),
			outputUsageNew(v.cmd).withRequired(v.required).createUsageWithTitle(nil))
	}

	if err := v.validateConflicts(matcher, conflicts); err != nil {
		return err
	}
	if !(v.cmd.IsSubcommandNegatesReqsSet() && hasSubcmd) {
		if err := v.validateRequired(matcher, conflicts); err != nil {
			return err
		}
	}

	return nil
}

func (v *parserValidator) validateConflicts(matcher *parserArgMatcher, conflicts *parserConflicts) *Error {
	if err := v.validateExclusive(matcher); err != nil {
		return err
	}

	ids, matched := matcherArgs(matcher)
	for i, argId := range ids {
		if !matched[i].checkExplicit(argPredicateIsPresent) {
			continue
		}
		if v.cmd.find(argId) == nil {
			continue
		}
		conflicts := conflicts.gatherConflicts(v.cmd, argId)
		if err := v.buildConflictErr(argId, conflicts, matcher); err != nil {
			return err
		}
	}

	return nil
}

func (v *parserValidator) validateExclusive(matcher *parserArgMatcher) *Error {
	ids, matched := matcherArgs(matcher)
	argsCount := 0
	for i, argId := range ids {
		if matched[i].checkExplicit(argPredicateIsPresent) &&
			// Avoid including our own groups by checking none of them.  If a group is present, the
			// args for the group will be.
			v.cmd.find(argId) != nil {
			argsCount++
		}
	}
	if argsCount <= 1 {
		// Nothing present to conflict with
		return nil
	}

	for i, id := range ids {
		if !matched[i].checkExplicit(argPredicateIsPresent) {
			continue
		}
		arg := v.cmd.find(id)
		// Find `arg`s which are exclusive but also appear with other args.
		if arg != nil && arg.IsExclusiveSet() && argsCount > 1 {
			// Throw an error for the first conflict found.
			return errorArgumentConflict(v.cmd, arg.String(), []string{},
				outputUsageNew(v.cmd).withRequired(v.required).createUsageWithTitle(nil))
		}
	}
	return nil
}

func (v *parserValidator) buildConflictErr(name Id, conflictIds []Id, matcher *parserArgMatcher) *Error {
	if len(conflictIds) == 0 {
		return nil
	}

	var unrolled utilFlatSet[Id]
	for _, cId := range conflictIds {
		if v.cmd.findGroup(cId) != nil {
			unrolled.extend(v.cmd.unrollArgsInGroup(cId))
		} else {
			unrolled.insert(cId)
		}
	}
	conflictIds = unrolled.intoVec()
	var conflicts []string
	for _, cId := range conflictIds {
		cArg := v.cmd.find(cId)
		if cArg == nil {
			rs.PanicAt(1, internalErrorMsg)
		}
		conflicts = append(conflicts, cArg.String())
	}

	formerArg := v.cmd.find(name)
	if formerArg == nil {
		rs.PanicAt(1, internalErrorMsg)
	}
	usg := v.buildConflictErrUsage(matcher, conflictIds)
	return errorArgumentConflict(v.cmd, formerArg.String(), conflicts, usg)
}

func (v *parserValidator) buildConflictErrUsage(matcher *parserArgMatcher, conflictingKeys []Id) rs.Option[BuilderStyledStr] {
	var usedFiltered []Id
	ids, matched := matcherArgs(matcher)
	for i, n := range ids {
		if !matched[i].checkExplicit(argPredicateIsPresent) {
			continue
		}
		// Filter out the args we don't want to specify.
		a := v.cmd.find(n)
		if a == nil || a.IsHideSet() {
			continue
		}
		if idsContain(conflictingKeys, n) {
			continue
		}
		usedFiltered = append(usedFiltered, n)
	}
	var required []Id
	for _, key := range usedFiltered {
		arg := v.cmd.find(key)
		if arg == nil {
			continue
		}
		for _, item := range arg.requires {
			if !idsContain(usedFiltered, item.id) && !idsContain(conflictingKeys, item.id) {
				required = append(required, item.id)
			}
		}
	}
	required = append(required, usedFiltered...)
	return outputUsageNew(v.cmd).withRequired(v.required).createUsageWithTitle(required)
}

func (v *parserValidator) gatherRequires(matcher *parserArgMatcher) {
	ids, matchedArgs := matcherArgs(matcher)
	for i, name := range ids {
		matched := matchedArgs[i]
		if !matched.checkExplicit(argPredicateIsPresent) {
			continue
		}
		if arg := v.cmd.find(name); arg != nil {
			isRelevant := func(r *argRequire) (Id, bool) {
				required := matched.checkExplicit(r.predicate)
				return r.id, required
			}
			for _, req := range v.cmd.unrollArgRequires(isRelevant, arg.GetId()) {
				v.required.insert(req)
			}
		} else if g := v.cmd.findGroup(name); g != nil {
			for _, r := range g.requires {
				v.required.insert(r)
			}
		}
	}
}

func (v *parserValidator) validateRequired(matcher *parserArgMatcher, conflicts *parserConflicts) *Error {
	v.gatherRequires(matcher)

	var missingRequired []Id
	highestIndex := 0

	isExclusivePresent := false
	ids, matched := matcherArgs(matcher)
	for i, id := range ids {
		if !matched[i].checkExplicit(argPredicateIsPresent) {
			continue
		}
		if arg := v.cmd.find(id); arg != nil && arg.IsExclusiveSet() {
			isExclusivePresent = true
			break
		}
	}
	for _, argOrGroup := range v.required.iter() {
		if matcher.checkExplicit(argOrGroup, argPredicateIsPresent) {
			continue
		}
		if arg := v.cmd.find(argOrGroup); arg != nil {
			if !isExclusivePresent && !v.isMissingRequiredOk(arg, conflicts) {
				missingRequired = append(missingRequired, arg.GetId())
				if !arg.IsLastSet() {
					highestIndex = max(highestIndex, arg.GetIndex().UnwrapOr(0))
				}
			}
		} else if group := v.cmd.findGroup(argOrGroup); group != nil {
			present := false
			for _, a := range v.cmd.unrollArgsInGroup(group.id) {
				if matcher.checkExplicit(a, argPredicateIsPresent) {
					present = true
					break
				}
			}
			if !present {
				missingRequired = append(missingRequired, group.GetId())
			}
		}
	}

	// Validate the conditionally required args
	for _, a := range v.cmd.GetArguments() {
		if matcher.checkExplicit(a.GetId(), argPredicateIsPresent) {
			continue
		}
		required := false
		for _, rIf := range a.rIfs {
			if matcher.checkExplicit(rIf.id, BuilderArgPredicateFromStr(rIf.val)) {
				required = true
			}
		}

		matchAll := true
		for _, rIf := range a.rIfsAll {
			if !matcher.checkExplicit(rIf.id, BuilderArgPredicateFromStr(rIf.val)) {
				matchAll = false
				break
			}
		}
		if matchAll && len(a.rIfsAll) != 0 {
			required = true
		}

		if (len(a.rUnless) != 0 || len(a.rUnlessAll) != 0) &&
			v.failsArgRequiredUnless(a, matcher) {
			required = true
		}

		if !isExclusivePresent && required {
			missingRequired = append(missingRequired, a.GetId())
			if !a.IsLastSet() {
				highestIndex = max(highestIndex, a.GetIndex().UnwrapOr(0))
			}
		}
	}

	// For display purposes, include all of the preceding positional arguments
	if !v.cmd.IsAllowMissingPositionalSet() {
		for _, pos := range v.cmd.GetPositionals() {
			if matcher.checkExplicit(pos.GetId(), argPredicateIsPresent) {
				continue
			}
			// `pos.get_index() < Some(highest_index)`: None is less than any Some.
			index, ok := pos.GetIndex().Get()
			if !ok || index < highestIndex {
				missingRequired = append(missingRequired, pos.GetId())
			}
		}
	}

	if len(missingRequired) != 0 {
		if err := v.missingRequiredError(matcher, missingRequired); err != nil {
			return err
		}
	}

	return nil
}

func (v *parserValidator) isMissingRequiredOk(a *Arg, conflicts *parserConflicts) bool {
	if len(conflicts.gatherConflicts(v.cmd, a.GetId())) != 0 {
		return true
	}
	for _, groupId := range v.cmd.groupsForArg(a.GetId()) {
		if len(conflicts.gatherConflicts(v.cmd, groupId)) != 0 {
			return true
		}
	}
	return false
}

// Failing a required unless means, the arg's "unless" wasn't present, and neither were they
func (v *parserValidator) failsArgRequiredUnless(a *Arg, matcher *parserArgMatcher) bool {
	exists := func(id Id) bool { return matcher.checkExplicit(id, argPredicateIsPresent) }

	allExist := true
	for _, id := range a.rUnlessAll {
		if !exists(id) {
			allExist = false
			break
		}
	}
	anyExists := false
	for _, id := range a.rUnless {
		if exists(id) {
			anyExists = true
			break
		}
	}
	return (len(a.rUnlessAll) == 0 || !allExist) && !anyExists
}

// `req_args`: an arg to include in the error even if not used
func (v *parserValidator) missingRequiredError(matcher *parserArgMatcher, rawReqArgs []Id) *Error {
	usg := outputUsageNew(v.cmd).withRequired(v.required)

	var reqArgs []string
	for _, s := range usg.getRequiredUsageFrom(rawReqArgs, matcher, true) {
		reqArgs = append(reqArgs, s.String())
	}

	var used []Id
	ids, matched := matcherArgs(matcher)
	for i, n := range ids {
		if !matched[i].checkExplicit(argPredicateIsPresent) {
			continue
		}
		// Filter out the args we don't want to specify.
		if a := v.cmd.find(n); a == nil || a.IsHideSet() {
			continue
		}
		used = append(used, n)
	}
	used = append(used, rawReqArgs...)

	return errorMissingRequiredArgument(v.cmd, reqArgs, usg.createUsageWithTitle(used))
}

// parserConflicts is Conflicts.
type parserConflicts struct {
	potential utilFlatMap[Id, []Id]
}

func parserConflictsWithArgs(cmd *Command, matcher *parserArgMatcher) *parserConflicts {
	conflicts := &parserConflicts{}
	ids, matched := matcherArgs(matcher)
	for i, id := range ids {
		if !matched[i].checkExplicit(argPredicateIsPresent) {
			continue
		}
		conf := gatherDirectConflicts(cmd, id)
		conflicts.potential.insertUnchecked(id, conf)
	}
	return conflicts
}

func (c *parserConflicts) gatherConflicts(cmd *Command, argId Id) []Id {
	var conflicts []Id

	var argIdConflicts []Id
	if direct, ok := c.getDirectConflicts(argId).Get(); ok {
		argIdConflicts = direct
	} else {
		// `is_missing_required_ok` is a case where we check not-present args for conflicts
		argIdConflicts = gatherDirectConflicts(cmd, argId)
	}
	for i, otherArgId := range c.potential.keys {
		otherArgIdConflicts := c.potential.values[i]
		if argId == otherArgId {
			continue
		}

		if idsContain(argIdConflicts, otherArgId) {
			conflicts = append(conflicts, otherArgId)
		}
		if idsContain(otherArgIdConflicts, argId) {
			conflicts = append(conflicts, otherArgId)
		}
	}

	return conflicts
}

func (c *parserConflicts) getDirectConflicts(argId Id) rs.Option[[]Id] {
	if found := c.potential.get(argId); found != nil {
		return rs.Some(*found)
	}
	return rs.None[[]Id]()
}

func gatherDirectConflicts(cmd *Command, id Id) []Id {
	var conf []Id
	if arg := cmd.find(id); arg != nil {
		conf = gatherArgDirectConflicts(cmd, arg)
	} else if group := cmd.findGroup(id); group != nil {
		conf = gatherGroupDirectConflicts(group)
	} else {
		if rs.DebugAssertions {
			rs.PanicAt(1, "id="+strDebug(string(id))+" is unknown")
		}
		conf = []Id{}
	}
	return conf
}

func gatherArgDirectConflicts(cmd *Command, arg *Arg) []Id {
	conf := cloneIds(arg.blacklist)
	for _, groupId := range cmd.groupsForArg(arg.GetId()) {
		group := cmd.findGroup(groupId)
		if group == nil {
			rs.PanicAt(1, internalErrorMsg)
		}
		conf = append(conf, group.conflicts...)
		if !group.multiple {
			for _, memberId := range group.args {
				if memberId != arg.GetId() {
					conf = append(conf, memberId)
				}
			}
		}
	}

	// Overrides are implicitly conflicts
	conf = append(conf, arg.overrides...)

	return conf
}

func gatherGroupDirectConflicts(group *ArgGroup) []Id {
	return cloneIds(group.conflicts)
}

func parserGetPossibleValuesCli(a *Arg) []BuilderPossibleValue {
	if !a.isTakesValueSet() {
		return nil
	}
	return a.GetValueParser().PossibleValues().UnwrapOrDefault()
}
