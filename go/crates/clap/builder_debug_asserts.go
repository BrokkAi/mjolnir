// Port of clap_builder 4.6.0 src/builder/debug_asserts.rs

package clap

import (
	"sort"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// debugAssertPanic is `panic!` (and a failed `assert!`) with a message.
func debugAssertPanic(message string) {
	rs.PanicAt(2, message)
}

// debugAssertEqPanic is a failed `assert_eq!` with a message.
func debugAssertEqPanic(message, left, right string) {
	rs.PanicAt(2, "assertion `left == right` failed: "+message+"\n  left: "+left+"\n right: "+right)
}

func debugAssertsAssertApp(cmd *Command) {
	var shortFlags []debugFlag
	var longFlags []debugFlag

	// Invalid version flag settings
	if cmd.GetVersion().IsNone() && cmd.GetLongVersion().IsNone() {
		// PropagateVersion is meaningless if there is no version
		if cmd.IsPropagateVersionSet() {
			debugAssertPanic("Command " + cmd.GetName() + ": No version information via Command::version or Command::long_version to propagate")
		}

		// Used `Command::mut_arg("version", ..) but did not provide any version information to display
		var versionNeeded []string
		for _, x := range cmd.GetArguments() {
			if x.GetAction() == ArgAction_Version {
				versionNeeded = append(versionNeeded, strDebug(string(x.GetId())))
			}
		}

		if len(versionNeeded) != 0 {
			debugAssertEqPanic("Command "+cmd.GetName()+": `ArgAction::Version` used without providing Command::version or Command::long_version",
				"["+strings.Join(versionNeeded, ", ")+"]", "[]")
		}
	}

	for _, sc := range cmd.GetSubcommands() {
		if s, ok := sc.GetShortFlag().Get(); ok {
			shortFlags = append(shortFlags, debugFlag{command: true, flag: "-" + string(s), name: sc.GetName()})
		}

		for _, shortAlias := range sc.GetAllShortFlagAliases() {
			shortFlags = append(shortFlags, debugFlag{command: true, flag: "-" + string(shortAlias), name: sc.GetName()})
		}

		if l, ok := sc.GetLongFlag().Get(); ok {
			if strings.HasPrefix(l, "-") {
				debugAssertPanic("Command " + sc.GetName() + ": long_flag " + strDebug(l) + " must not start with a `-`, that will be handled by the parser")
			}
			longFlags = append(longFlags, debugFlag{command: true, flag: "--" + l, name: sc.GetName()})
		}

		for _, longAlias := range sc.GetAllLongFlagAliases() {
			longFlags = append(longFlags, debugFlag{command: true, flag: "--" + longAlias, name: sc.GetName()})
		}
	}

	for _, arg := range cmd.GetArguments() {
		debugAssertsAssertArg(arg)

		if cmd.IsMulticallSet() {
			debugAssertPanic("Command " + cmd.GetName() + ": Arguments like " + arg.GetId().AsStr() + " cannot be set on a multicall command")
		}

		if s, ok := arg.GetShort().Get(); ok {
			shortFlags = append(shortFlags, debugFlag{flag: "-" + string(s), name: arg.GetId().AsStr()})
		}

		for _, shortAlias := range arg.shortAliases {
			shortFlags = append(shortFlags, debugFlag{flag: "-" + string(shortAlias.name), name: arg.GetId().AsStr()})
		}

		if l, ok := arg.GetLong().Get(); ok {
			if strings.HasPrefix(l, "-") {
				debugAssertPanic("Argument " + arg.GetId().AsStr() + ": long " + strDebug(l) + " must not start with a `-`, that will be handled by the parser")
			}
			longFlags = append(longFlags, debugFlag{flag: "--" + l, name: arg.GetId().AsStr()})
		}

		for _, longAlias := range arg.aliases {
			longFlags = append(longFlags, debugFlag{flag: "--" + longAlias.name, name: arg.GetId().AsStr()})
		}

		// Name conflicts
		if first, second, ok := cmd.twoArgsOf(func(x *Arg) bool { return x.GetId() == arg.GetId() }); ok {
			debugAssertPanic("Command " + cmd.GetName() + ": Argument names must be unique, but '" + arg.GetId().AsStr() +
				"' is in use by more than one argument or group" + duplicateTip(cmd, first, second))
		}

		// Long conflicts
		if l, ok := arg.GetLong().Get(); ok {
			if first, second, ok := cmd.twoArgsOf(func(x *Arg) bool { return optionIs(x.GetLong(), l) }); ok {
				debugAssertPanic("Command " + cmd.GetName() + ": Long option names must be unique for each argument, " +
					"but '--" + l + "' is in use by both '" + first.GetId().AsStr() + "' and '" + second.GetId().AsStr() + "'" +
					duplicateTip(cmd, first, second))
			}
		}

		// Short conflicts
		if s, ok := arg.GetShort().Get(); ok {
			if first, second, ok := cmd.twoArgsOf(func(x *Arg) bool { return optionIs(x.GetShort(), s) }); ok {
				debugAssertPanic("Command " + cmd.GetName() + ": Short option names must be unique for each argument, " +
					"but '-" + string(s) + "' is in use by both '" + first.GetId().AsStr() + "' and '" + second.GetId().AsStr() + "'" +
					duplicateTip(cmd, first, second))
			}
		}

		// Index conflicts
		if idx, ok := arg.index.Get(); ok {
			if first, second, ok := cmd.twoArgsOf(func(x *Arg) bool { return x.IsPositional() && optionIs(x.GetIndex(), idx) }); ok {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument '" + first.GetId().AsStr() + "' has the same index as '" +
					second.GetId().AsStr() + "' and they are both positional arguments\n\n\t " +
					"Use `Arg::num_args(1..)` to allow one " +
					"positional argument to take multiple values")
			}
		}

		// requires, r_if, r_unless
		for _, req := range arg.requires {
			if arg.id == req.id {
				debugAssertPanic("Argument " + arg.GetId().AsStr() + " cannot require itself")
			}

			if !cmd.idExists(req.id) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.id.AsStr() +
					"' specified in 'requires*' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		for _, req := range arg.rIfs {
			if arg.IsRequiredSet() {
				debugAssertPanic("Argument " + arg.GetId().AsStr() + ": `required` conflicts with `required_if_eq*`")
			}
			if !cmd.idExists(req.id) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.id.AsStr() +
					"' specified in 'required_if_eq*' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		for _, req := range arg.rIfsAll {
			if arg.IsRequiredSet() {
				debugAssertPanic("Argument " + arg.GetId().AsStr() + ": `required` conflicts with `required_if_eq_all`")
			}
			if !cmd.idExists(req.id) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.id.AsStr() +
					"' specified in 'required_if_eq_all' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		for _, req := range arg.rUnless {
			if arg.IsRequiredSet() {
				debugAssertPanic("Argument " + arg.GetId().AsStr() + ": `required` conflicts with `required_unless*`")
			}
			if !cmd.idExists(req) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.AsStr() +
					"' specified in 'required_unless*' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		for _, req := range arg.rUnlessAll {
			if arg.IsRequiredSet() {
				debugAssertPanic("Argument " + arg.GetId().AsStr() + ": `required` conflicts with `required_unless*`")
			}
			if !cmd.idExists(req) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.AsStr() +
					"' specified in 'required_unless*' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		// blacklist
		for _, req := range arg.blacklist {
			if !cmd.idExists(req) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.AsStr() +
					"' specified in 'conflicts_with*' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		// overrides
		for _, req := range arg.overrides {
			if !cmd.idExists(req) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument or group '" + req.AsStr() +
					"' specified in 'overrides_with*' for '" + arg.GetId().AsStr() + "' does not exist")
			}
		}

		if arg.IsLastSet() {
			if arg.GetLong().IsSome() {
				debugAssertPanic("Command " + cmd.GetName() + ": Flags or Options cannot have last(true) set. '" +
					arg.GetId().AsStr() + "' has both a long and last(true) set.")
			}
			if arg.GetShort().IsSome() {
				debugAssertPanic("Command " + cmd.GetName() + ": Flags or Options cannot have last(true) set. '" +
					arg.GetId().AsStr() + "' has both a short and last(true) set.")
			}
		}

		if arg.IsRequiredSet() && arg.IsGlobalSet() {
			debugAssertPanic("Command " + cmd.GetName() + ": Global arguments cannot be required.\n\n\t'" +
				arg.GetId().AsStr() + "' is marked as both global and required")
		}

		if arg.GetValueHint() == ValueHint_CommandWithArguments {
			if !arg.IsPositional() {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument '" + arg.GetId().AsStr() +
					"' has hint CommandWithArguments and must be positional.")
			}

			if !(arg.IsTrailingVarArgSet() || arg.IsLastSet()) {
				debugAssertPanic("Command " + cmd.GetName() + ": Positional argument '" + arg.GetId().AsStr() +
					"' has hint CommandWithArguments, so Command must have `trailing_var_arg(true)` or `last(true)` set.")
			}
		}
	}

	for _, group := range cmd.GetGroups() {
		// Name conflicts
		count := 0
		for _, x := range cmd.GetGroups() {
			if x.id == group.id {
				count++
			}
		}
		if count >= 2 {
			debugAssertPanic("Command " + cmd.GetName() + ": Argument group name must be unique\n\n\t'" +
				group.GetId().AsStr() + "' is already in use")
		}

		// Groups should not have naming conflicts with Args
		for _, x := range cmd.GetArguments() {
			if x.GetId() == group.GetId() {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument group name '" + group.GetId().AsStr() +
					"' must not conflict with argument name")
			}
		}

		for _, arg := range group.args {
			// Args listed inside groups should exist
			exists := false
			for _, x := range cmd.GetArguments() {
				if x.GetId() == arg {
					exists = true
					break
				}
			}
			if !exists {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument group '" + group.GetId().AsStr() +
					"' contains non-existent argument '" + arg.AsStr() + "'")
			}
		}

		for _, arg := range group.requires {
			// Args listed inside groups should exist
			if !cmd.idExists(arg) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument group '" + group.GetId().AsStr() +
					"' requires non-existent '" + arg.AsStr() + "' id")
			}
		}

		for _, arg := range group.conflicts {
			// Args listed inside groups should exist
			if !cmd.idExists(arg) {
				debugAssertPanic("Command " + cmd.GetName() + ": Argument group '" + group.GetId().AsStr() +
					"' conflicts with non-existent '" + arg.AsStr() + "' id")
			}
		}
	}

	// Conflicts between flags and subcommands

	debugFlagsSortUnstable(longFlags)
	debugFlagsSortUnstable(shortFlags)

	detectDuplicateFlags(longFlags, "long")
	detectDuplicateFlags(shortFlags, "short")

	var subs utilFlatSet[string]
	for _, sc := range cmd.GetSubcommands() {
		if !subs.insert(sc.GetName()) {
			debugAssertPanic("Command " + cmd.GetName() + ": command name `" + sc.GetName() + "` is duplicated")
		}
		for _, alias := range sc.GetAllAliases() {
			if !subs.insert(alias) {
				debugAssertPanic("Command " + cmd.GetName() + ": command `" + sc.GetName() + "` alias `" + alias + "` is duplicated")
			}
		}
	}

	verifyPositionals(cmd)

	if helpTemplate, ok := cmd.getHelpTemplate().Get(); ok {
		if strings.Contains(helpTemplate.String(), "{flags}") {
			debugAssertPanic("Command " + cmd.GetName() + ": " +
				"`{flags}` template variable was removed in clap3, they are now included in `{options}`")
		}
		if strings.Contains(helpTemplate.String(), "{unified}") {
			debugAssertPanic("Command " + cmd.GetName() + ": " +
				"`{unified}` template variable was removed in clap3, use `{options}` instead")
		}
	}

	cmd.panicOnMissingHelp(cmd.isHelpExpectedSet())
	assertAppFlags(cmd)
}

func duplicateTip(cmd *Command, first *Arg, second *Arg) string {
	if !cmd.IsDisableHelpFlagSet() &&
		(first.GetId() == idHelp || second.GetId() == idHelp) {
		return " (call `cmd.disable_help_flag(true)` to remove the auto-generated `--help`)"
	} else if !cmd.IsDisableVersionFlagSet() &&
		(first.GetId() == idVersion || second.GetId() == idVersion) {
		return " (call `cmd.disable_version_flag(true)` to remove the auto-generated `--version`)"
	}
	return ""
}

// debugFlag is Flag: Command(String, &str) when command is set, otherwise
// Arg(String, &str). Flags compare by their text only.
type debugFlag struct {
	command bool
	flag    string
	name    string
}

// debugFlagsSortUnstable is `sort_unstable` on the flags. Elements that
// compare equal are duplicates detect_duplicate_flags reports, and the
// pair it reports is the same whichever of them comes first except in
// which name each side has; the order among equal elements is Rust's
// sort_unstable order for these small slices: insertion sort keeps them
// in their original order.
func debugFlagsSortUnstable(flags []debugFlag) {
	sort.SliceStable(flags, func(i, j int) bool { return flags[i].flag < flags[j].flag })
}

func detectDuplicateFlags(flags []debugFlag, shortOrLong string) {
	for i := 0; i+1 < len(flags); i++ {
		one, two := flags[i], flags[i+1]
		if one.flag != two.flag {
			continue
		}
		switch {
		case one.command && two.command && one.name != two.name:
			debugAssertPanic("the '" + one.flag + "' " + shortOrLong + " flag is specified for both '" + one.name + "' and '" + two.name + "' subcommands")
		case !one.command && !two.command && one.name != two.name:
			debugAssertPanic(shortOrLong + " option names must be unique, but '" + one.flag + "' is in use by both '" + one.name + "' and '" + two.name + "'")
		case !one.command && two.command:
			debugAssertPanic("the '" + one.flag + "' " + shortOrLong + " flag for the '" + one.name + "' argument conflicts with the short flag " +
				"for '" + two.name + "' subcommand")
		case one.command && !two.command:
			debugAssertPanic("the '" + one.flag + "' " + shortOrLong + " flag for the '" + two.name + "' argument conflicts with the short flag " +
				"for '" + one.name + "' subcommand")
		}
	}
}

func assertAppFlags(cmd *Command) {
	if cmd.IsMulticallSet() {
		s := ""
		if cmd.IsNoBinaryNameSet() {
			s += "  AppSettings::is_no_binary_name_set conflicts with AppSettings::is_multicall_set.\n"
		}
		if s != "" {
			debugAssertPanic(cmd.GetName() + "\n" + s)
		}
	}
}

func verifyPositionals(cmd *Command) bool {
	// Because you must wait until all arguments have been supplied, this is the first chance
	// to make assertions on positional argument indexes
	//
	// First we verify that the index highest supplied index, is equal to the number of
	// positional arguments to verify there are no gaps (i.e. supplying an index of 1 and 3
	// but no 2)

	highestIdx := 0
	numP := 0
	for i := range cmd.getKeymap().keys {
		key := cmd.getKeymap().keys[i].key
		if key.isPosition() {
			highestIdx = max(highestIdx, key.position)
			numP++
		}
	}

	if highestIdx != numP {
		debugAssertPanic("Found positional argument whose index is " + strconv.Itoa(highestIdx) + " but there " +
			"are only " + strconv.Itoa(numP) + " positional arguments defined")
	}

	for _, arg := range cmd.GetArguments() {
		if arg.index.UnwrapOr(0) == highestIdx {
			if arg.IsTrailingVarArgSet() && arg.IsLastSet() {
				debugAssertPanic(cmd.GetName() + ":" + arg.GetId().AsStr() + ": `Arg::trailing_var_arg` and `Arg::last` cannot be used together")
			}

			if arg.IsTrailingVarArgSet() {
				if !arg.isMultiple() {
					debugAssertPanic(cmd.GetName() + ":" + arg.GetId().AsStr() + ": `Arg::trailing_var_arg` must accept multiple values")
				}
			}
		} else {
			if arg.IsTrailingVarArgSet() {
				debugAssertPanic(cmd.GetName() + ":" + arg.GetId().AsStr() + ": `Arg::trailing_var_arg` can only apply to last positional")
			}
		}
	}

	// Next we verify that only the highest index has takes multiple arguments (if any)
	onlyHighest := func(a *Arg) bool { return a.isMultiple() && (a.GetIndex().UnwrapOr(0) != highestIdx) }
	anyOnlyHighest := false
	for _, p := range cmd.GetPositionals() {
		if onlyHighest(p) {
			anyOnlyHighest = true
			break
		}
	}
	if anyOnlyHighest {
		// First we make sure if there is a positional that allows multiple values
		// the one before it (second to last) has one of these:
		//  * a value terminator
		//  * ArgSettings::Last
		//  * The last arg is Required

		// We can't pass the closure (it.next()) to the macro directly because each call to
		// find() (iterator, not macro) gets called repeatedly.
		last := cmd.getKeymap().getPosition(highestIdx)
		secondToLast := cmd.getKeymap().getPosition(highestIdx - 1)
		if last == nil || secondToLast == nil {
			rs.PanicAt(1, "Arg with key Position("+strconv.Itoa(highestIdx)+") not found")
		}

		// Either the final positional is required
		// Or the second to last has a terminator or .last(true) set
		ok := last.IsRequiredSet() ||
			(secondToLast.terminator.IsSome() || secondToLast.IsLastSet()) ||
			last.IsLastSet()
		if !ok {
			debugAssertPanic("Positional argument `" + last.String() + "` *must* have `required(true)` or `last(true)` set " +
				"because a prior positional argument (`" + secondToLast.String() + "`) has `num_args(1..)`")
		}

		// We make sure if the second to last is Multiple the last is ArgSettings::Last
		ok = secondToLast.isMultiple() || last.IsLastSet()
		if !ok {
			debugAssertPanic("Only the last positional argument, or second to last positional " +
				"argument may be set to `.num_args(1..)`")
		}

		// Next we check how many have both Multiple and not a specific number of values set
		count := 0
		for _, p := range cmd.GetPositionals() {
			if p.isMultipleValuesSet() &&
				p.GetValueTerminator().IsNone() &&
				!p.GetNumArgs().Expect(internalErrorMsg).isFixed() {
				count++
			}
		}
		ok = count <= 1 ||
			(last.IsLastSet() &&
				last.isMultiple() &&
				secondToLast.isMultiple() &&
				count == 2)
		if !ok {
			debugAssertPanic("Only one positional argument with `.num_args(1..)` set is allowed per " +
				"command, unless the second one also has .last(true) set")
		}
	}

	found := false

	if cmd.IsAllowMissingPositionalSet() {
		// Check that if a required positional argument is found, all positions with a lower
		// index are also required.
		foundx2 := false

		for _, p := range cmd.GetPositionals() {
			if foundx2 && !p.IsRequiredSet() {
				if !p.IsRequiredSet() {
					debugAssertPanic("Found non-required positional argument with a lower " +
						"index than a required positional argument by two or more: " + strDebug(p.GetId().AsStr()) + " " +
						"index " + optionIntDebug(p.GetIndex()))
				}
			} else if p.IsRequiredSet() && !p.IsLastSet() {
				// Args that .last(true) don't count since they can be required and have
				// positionals with a lower index that aren't required
				// Imagine: prog <req1> [opt1] -- <req2>
				// Both of these are valid invocations:
				//      $ prog r1 -- r2
				//      $ prog r1 o1 -- r2
				if found {
					foundx2 = true
					continue
				}
				found = true
			} else {
				found = false
			}
		}
	} else {
		// Check that if a required positional argument is found, all positions with a lower
		// index are also required
		for n := numP; n >= 1; n-- {
			p := cmd.getKeymap().getPosition(n)
			if p == nil {
				continue
			}
			if found {
				if !p.IsRequiredSet() {
					debugAssertPanic("Found non-required positional argument with a lower " +
						"index than a required positional argument: " + strDebug(p.GetId().AsStr()) + " index " + optionIntDebug(p.GetIndex()))
				}
			} else if p.IsRequiredSet() && !p.IsLastSet() {
				// Args that .last(true) don't count since they can be required and have
				// positionals with a lower index that aren't required
				// Imagine: prog <req1> [opt1] -- <req2>
				// Both of these are valid invocations:
				//      $ prog r1 -- r2
				//      $ prog r1 o1 -- r2
				found = true
			}
		}
	}
	lastCount := 0
	for _, p := range cmd.GetPositionals() {
		if p.IsLastSet() {
			lastCount++
		}
	}
	if lastCount >= 2 {
		debugAssertPanic("Only one positional argument may have last(true) set. Found two.")
	}
	anyRequiredLast := false
	for _, p := range cmd.GetPositionals() {
		if p.IsLastSet() && p.IsRequiredSet() {
			anyRequiredLast = true
			break
		}
	}
	if anyRequiredLast &&
		cmd.HasSubcommands() &&
		!cmd.IsSubcommandNegatesReqsSet() {
		debugAssertPanic("Having a required positional argument with .last(true) set *and* child " +
			"subcommands without setting SubcommandsNegateReqs isn't compatible.")
	}

	return true
}

// optionIntDebug is `{:?}` of an Option<usize>.
func optionIntDebug(o rs.Option[int]) string {
	if v, ok := o.Get(); ok {
		return "Some(" + strconv.Itoa(v) + ")"
	}
	return "None"
}

func debugAssertsAssertArg(arg *Arg) {
	// Self conflict
	// TODO: this check should be recursive
	if idsContain(arg.blacklist, arg.GetId()) {
		debugAssertPanic("Argument '" + arg.GetId().AsStr() + "' cannot conflict with itself")
	}

	numArgs := arg.GetNumArgs().UnwrapOr(BuilderValueRangeFromUsize(1))
	if !(numArgs.MaxValues() <= arg.GetAction().maxNumArgs().MaxValues()) {
		debugAssertPanic("Argument `" + arg.GetId().AsStr() + "`'s action " + arg.GetAction().debugString() +
			" is incompatible with `num_args(" + numArgs.String() + ")`")
	}
	if actionTypeId, ok := arg.GetAction().valueTypeId().Get(); ok {
		if !actionTypeId.equal(arg.GetValueParser().TypeId()) {
			debugAssertEqPanic("Argument `"+arg.GetId().AsStr()+"`'s selected action "+arg.GetAction().debugString()+
				" contradicts `value_parser` ("+arg.GetValueParser().debugString()+")",
				actionTypeId.debugString(), arg.GetValueParser().TypeId().debugString())
		}
	}

	if arg.GetValueHint() != ValueHint_Unknown {
		if !arg.isTakesValueSet() {
			debugAssertPanic("Argument '" + arg.GetId().AsStr() + "' has value hint but takes no value")
		}

		if arg.GetValueHint() == ValueHint_CommandWithArguments {
			if !arg.isMultipleValuesSet() {
				debugAssertPanic("Argument '" + arg.GetId().AsStr() + "' uses hint CommandWithArguments and must accept multiple values")
			}
		}
	}

	if arg.index.IsSome() {
		if !arg.IsPositional() {
			debugAssertPanic("Argument '" + arg.GetId().AsStr() + "' is a positional argument and can't have short or long name versions")
		}
		if !arg.isTakesValueSet() {
			suffix := ""
			if arg.GetId() == idHelp {
				suffix = " (`mut_arg` no longer works with implicit `--help`)"
			} else if arg.GetId() == idVersion {
				suffix = " (`mut_arg` no longer works with implicit `--version`)"
			}
			debugAssertPanic("Argument '" + arg.GetId().AsStr() + "' is positional and it must take a value but action is " +
				arg.GetAction().debugString() + suffix)
		}
	}

	numVals := arg.GetNumArgs().Expect(internalErrorMsg)
	// This can be the cause of later asserts, so put this first
	if numVals != BuilderValueRangeEmpty {
		// HACK: Don't check for flags to make the derive easier
		numValNames := len(arg.GetValueNames().UnwrapOrDefault())
		if numVals.MaxValues() < numValNames {
			debugAssertPanic("Argument " + arg.GetId().AsStr() + ": Too many value names (" + strconv.Itoa(numValNames) +
				") compared to `num_args` (" + numVals.String() + ")")
		}
	}

	if numVals.isMultiple() != arg.isMultipleValuesSet() {
		debugAssertEqPanic("Argument "+arg.GetId().AsStr()+": mismatch between `num_args` ("+numVals.String()+") and `multiple_values`",
			strconv.FormatBool(numVals.isMultiple()), strconv.FormatBool(arg.isMultipleValuesSet()))
	}

	if 1 < numVals.MinValues() {
		if arg.IsRequireEqualsSet() {
			debugAssertPanic("Argument " + arg.GetId().AsStr() + ": cannot accept more than 1 arg (num_args=" + numVals.String() + ") with require_equals")
		}
	}

	if numVals == BuilderValueRangeSingle {
		if arg.isMultipleValuesSet() {
			debugAssertPanic("Argument " + arg.GetId().AsStr() + ": mismatch between `num_args` and `multiple_values`")
		}
	}

	assertArgFlags(arg)
}

func assertArgFlags(arg *Arg) {
	checker := func(a bool, aName string, b bool, bName string) {
		if a {
			s := ""
			if !b {
				s += "  Arg::" + bName + " is required when Arg::" + aName + " is set.\n"
			}
			if s != "" {
				debugAssertPanic("Argument " + strDebug(arg.GetId().AsStr()) + "\n" + s)
			}
		}
	}

	checker(arg.IsHidePossibleValuesSet(), "is_hide_possible_values_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.IsAllowHyphenValuesSet(), "is_allow_hyphen_values_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.IsAllowNegativeNumbersSet(), "is_allow_negative_numbers_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.IsRequireEqualsSet(), "is_require_equals_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.IsLastSet(), "is_last_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.IsHideDefaultValueSet(), "is_hide_default_value_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.isMultipleValuesSet(), "is_multiple_values_set", arg.isTakesValueSet(), "is_takes_value_set")
	checker(arg.IsIgnoreCaseSet(), "is_ignore_case_set", arg.isTakesValueSet(), "is_takes_value_set")
}
