// Port of clap_builder 4.6.0 src/parser/parser.rs

package clap

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// parserParser is Parser.
type parserParser struct {
	cmd            *Command
	curIdx         int
	flagSubcmdAt   rs.Option[int]
	flagSubcmdSkip int
}

// Initializing Methods
func parserParserNew(cmd *Command) *parserParser {
	return &parserParser{
		cmd:            cmd,
		curIdx:         0,
		flagSubcmdAt:   rs.None[int](),
		flagSubcmdSkip: 0,
	}
}

// Parsing Methods

// The actual parsing function
func (p *parserParser) getMatchesWith(matcher *parserArgMatcher, rawArgs *lexRawArgs, argsCursor lexArgCursor) *Error {
	if err := p.parse(matcher, rawArgs, argsCursor); err != nil {
		if p.cmd.isIgnoreErrorsSet() {
			_ = p.addEnv(matcher)
			_ = p.addDefaults(matcher)
		}
		return err
	}
	if err := p.resolvePending(matcher); err != nil {
		return err
	}

	if err := p.addEnv(matcher); err != nil {
		return err
	}
	if err := p.addDefaults(matcher); err != nil {
		return err
	}
	return parserValidatorNew(p.cmd).validate(matcher)
}

// The actual parsing function
func (p *parserParser) parse(matcher *parserArgMatcher, rawArgs *lexRawArgs, argsCursor lexArgCursor) *Error {
	// Verify all positional assertions pass

	subcmdName := rs.None[string]()
	keepState := false
	parseState := parserParseState{kind: parserParseState_ValuesDone}
	posCounter := 1

	// Already met any valid arg(then we shouldn't expect subcommands after it).
	validArgFound := false
	// If the user already passed '--'. Meaning only positional args follow.
	trailingValues := false

	// Count of positional args
	positionalCount := 0
	for i := range p.cmd.getKeymap().keys {
		if p.cmd.getKeymap().keys[i].key.isPosition() {
			positionalCount++
		}
	}
	// If any arg sets .last(true)
	containsLast := false
	for _, x := range p.cmd.GetArguments() {
		if x.IsLastSet() {
			containsLast = true
			break
		}
	}

parseLoop:
	for {
		argOs, ok := rawArgs.next(&argsCursor).Get()
		if !ok {
			break
		}

		// Has the user already passed '--'? Meaning only positional args follow
		if !trailingValues {
			if p.cmd.IsSubcommandPrecedenceOverArgSet() ||
				!(parseState.kind == parserParseState_Opt || parseState.kind == parserParseState_Pos) {
				// Does the arg match a subcommand name, or any of its aliases (if defined)
				scName := p.possibleSubcommand(argOs, validArgFound)
				if scName, ok := scName.Get(); ok {
					if scName == "help" && !p.cmd.IsDisableHelpSubcommandSet() {
						err := p.parseHelpSubcommand(rawArgs.remaining(&argsCursor))
						return err
					}
					subcmdName = rs.Some(scName)
					break parseLoop
				}
			}

			if argOs.isEscape() {
				if (parseState.kind == parserParseState_Opt || parseState.kind == parserParseState_Pos) &&
					p.cmd.index(parseState.id).IsAllowHyphenValuesSet() {
					// ParseResult::MaybeHyphenValue, do nothing
				} else {
					if arg := p.cmd.getKeymap().getPosition(posCounter); arg != nil &&
						p.checkTerminator(arg, argOs.toValueOs()).IsSome() {
						// count as both an escape and terminator
						posCounter++
					}
					trailingValues = true
					matcher.startTrailing()
					continue
				}
			} else if long, ok := argOs.toLong().Get(); ok {
				parseResult, err := p.parseLongArg(matcher, long, &parseState, posCounter, &validArgFound)
				if err != nil {
					return err
				}
				switch parseResult.kind {
				case parserParseResult_NoArg:
					panic(rs.Unreachable("`to_long` always has the flag specified"))
				case parserParseResult_ValuesDone:
					parseState = parserParseState{kind: parserParseState_ValuesDone}
					continue
				case parserParseResult_Opt:
					parseState = parserParseState{kind: parserParseState_Opt, id: parseResult.id}
					continue
				case parserParseResult_FlagSubCommand:
					subcmdName = rs.Some(parseResult.name)
					break parseLoop
				case parserParseResult_EqualsNotProvided:
					_ = p.resolvePending(matcher)
					return errorNoEquals(p.cmd, parseResult.arg, outputUsageNew(p.cmd).createUsageWithTitle(nil))
				case parserParseResult_NoMatchingArg:
					_ = p.resolvePending(matcher)
					remainingArgs := rawArgs.remaining(&argsCursor)
					return p.didYouMeanError(parseResult.arg, matcher, remainingArgs, trailingValues)
				case parserParseResult_UnneededAttachedValue:
					_ = p.resolvePending(matcher)
					return errorTooManyValues(p.cmd, parseResult.rest, parseResult.arg,
						outputUsageNew(p.cmd).createUsageWithTitle(parseResult.used))
				case parserParseResult_MaybeHyphenValue:
					// Maybe a hyphen value, do nothing.
				case parserParseResult_AttachedValueNotConsumed:
					panic(rs.Unreachable())
				default:
					panic(rs.Unreachable(parseResult.kind))
				}
			} else if shortArg, ok := argOs.toShort().Get(); ok {
				// Arg looks like a short flag, and not a possible number

				// Try to parse short args like normal, if allow_hyphen_values or
				// AllowNegativeNumbers is set, parse_short_arg will *not* throw
				// an error, and instead return Ok(None)
				parseResult, err := p.parseShortArg(matcher, shortArg, &parseState, posCounter, &validArgFound)
				if err != nil {
					return err
				}
				// If it's None, we then check if one of those two AppSettings was set
				switch parseResult.kind {
				case parserParseResult_NoArg:
					// Is a single dash `-`, try positional.
				case parserParseResult_ValuesDone:
					parseState = parserParseState{kind: parserParseState_ValuesDone}
					continue
				case parserParseResult_Opt:
					parseState = parserParseState{kind: parserParseState_Opt, id: parseResult.id}
					continue
				case parserParseResult_FlagSubCommand:
					// If there are more short flags to be processed, we should keep the state, and later
					// revisit the current group of short flags skipping the subcommand.
					if at, ok := p.flagSubcmdAt.Get(); ok {
						rawArgs.seek(&argsCursor, lexSeekFromCurrent(-1))
						// Since we are now saving the current state, the number of flags to skip during state recovery should
						// be the current index (`cur_idx`) minus ONE UNIT TO THE LEFT of the starting position.
						p.flagSubcmdSkip = p.curIdx - at + 1
						keepState = true
					} else {
						keepState = false
					}

					subcmdName = rs.Some(parseResult.name)
					break parseLoop
				case parserParseResult_EqualsNotProvided:
					_ = p.resolvePending(matcher)
					return errorNoEquals(p.cmd, parseResult.arg, outputUsageNew(p.cmd).createUsageWithTitle(nil))
				case parserParseResult_NoMatchingArg:
					_ = p.resolvePending(matcher)
					// We already know it looks like a flag
					suggestedTrailingArg := !trailingValues && p.cmd.hasPositionals()
					return errorUnknownArgument(p.cmd, parseResult.arg, rs.None[errorDidYouMean](),
						suggestedTrailingArg, outputUsageNew(p.cmd).createUsageWithTitle(nil))
				case parserParseResult_MaybeHyphenValue:
					// Maybe a hyphen value, do nothing.
				case parserParseResult_UnneededAttachedValue, parserParseResult_AttachedValueNotConsumed:
					panic(rs.Unreachable())
				default:
					panic(rs.Unreachable(parseResult.kind))
				}
			}

			if parseState.kind == parserParseState_Opt {
				id := parseState.id
				// Assume this is a value of a previous arg.

				// get the option so we can check the settings
				arg := p.cmd.index(id)
				var parseResult parserParseResult
				if result, ok := p.checkTerminator(arg, argOs.toValueOs()).Get(); ok {
					parseResult = result
				} else {
					trailingValues := false
					argValues := matcher.pendingValuesMut(id, rs.None[parserIdentifier](), trailingValues)
					argValues.rawVals = append(argValues.rawVals, argOs.toValueOs())
					if matcher.needsMoreVals(arg) {
						parseResult = parserParseResult{kind: parserParseResult_Opt, id: arg.GetId()}
					} else {
						parseResult = parserParseResult{kind: parserParseResult_ValuesDone}
					}
				}
				switch parseResult.kind {
				case parserParseResult_Opt:
					parseState = parserParseState{kind: parserParseState_Opt, id: parseResult.id}
				case parserParseResult_ValuesDone:
					parseState = parserParseState{kind: parserParseState_ValuesDone}
				default:
					panic(rs.Unreachable())
				}
				// get the next value from the iterator
				continue
			}
		}

		// Correct pos_counter.
		posCounter = func() int {
			isSecondToLast := posCounter+1 == positionalCount

			// The last positional argument, or second to last positional
			// argument may be set to .multiple_values(true) or `.multiple_occurrences(true)`
			lowIndexMults := false
			if isSecondToLast {
				anyMultiple := false
				for _, a := range p.cmd.GetPositionals() {
					if a.isMultiple() && (positionalCount != a.GetIndex().UnwrapOr(0)) {
						anyMultiple = true
						break
					}
				}
				if anyMultiple {
					positionals := p.cmd.GetPositionals()
					if len(positionals) > 0 {
						lowIndexMults = !positionals[len(positionals)-1].IsLastSet()
					}
				}
			}

			isTerminated := false
			if a := p.cmd.getKeymap().getPosition(posCounter); a != nil {
				isTerminated = a.GetValueTerminator().IsSome()
			}

			missingPos := p.cmd.IsAllowMissingPositionalSet() &&
				isSecondToLast &&
				!trailingValues

			if (lowIndexMults || missingPos) && !isTerminated {
				skipCurrent := true
				if n, ok := rawArgs.peek(&argsCursor).Get(); ok {
					var arg *Arg
					for _, a := range p.cmd.GetPositionals() {
						if index, ok := a.GetIndex().Get(); ok && index == posCounter {
							arg = a
							break
						}
					}
					if arg != nil {
						// If next value looks like a new_arg or it's a
						// subcommand, skip positional argument under current
						// pos_counter(which means current value cannot be a
						// positional argument with a value next to it), assume
						// current value matches the next arg.
						skipCurrent = p.isNewArg(n, arg) ||
							p.possibleSubcommand(n, validArgFound).IsSome()
					} else {
						skipCurrent = true
					}
				}

				if skipCurrent {
					return posCounter + 1
				}
				return posCounter
			} else if trailingValues &&
				(p.cmd.IsAllowMissingPositionalSet() || containsLast) {
				// Came to -- and one positional has .last(true) set, so we go immediately
				// to the last (highest index) positional
				return positionalCount
			}
			return posCounter
		}()

		if arg := p.cmd.getKeymap().getPosition(posCounter); arg != nil {
			if arg.IsLastSet() && !trailingValues {
				_ = p.resolvePending(matcher)
				// Its already considered a positional, we don't need to suggest turning it
				// into one
				suggestedTrailingArg := false
				return errorUnknownArgument(p.cmd, argOs.display(), rs.None[errorDidYouMean](),
					suggestedTrailingArg, outputUsageNew(p.cmd).createUsageWithTitle(nil))
			}

			if arg.IsTrailingVarArgSet() {
				trailingValues = true
			}

			if pendingId, ok := matcher.pendingArgId().Get(); !ok || pendingId != arg.GetId() || !arg.isMultipleValuesSet() {
				if err := p.resolvePending(matcher); err != nil {
					return err
				}
			}
			if parseResult, ok := p.checkTerminator(arg, argOs.toValueOs()).Get(); ok {
				if rs.DebugAssertions && parseResult.kind != parserParseResult_ValuesDone {
					rs.PanicAt(1, "assertion `left == right` failed")
				}
				posCounter++
				parseState = parserParseState{kind: parserParseState_ValuesDone}
			} else {
				argValues := matcher.pendingValuesMut(arg.GetId(), rs.Some(parserIdentifier_Index), trailingValues)
				argValues.rawVals = append(argValues.rawVals, argOs.toValueOs())

				// Only increment the positional counter if it doesn't allow multiples
				if !arg.isMultiple() {
					posCounter++
					parseState = parserParseState{kind: parserParseState_ValuesDone}
				} else {
					parseState = parserParseState{kind: parserParseState_Pos, id: arg.GetId()}
				}
			}
			validArgFound = true
		} else if externalParser, ok := p.cmd.GetExternalSubcommandValueParser().Get(); ok {
			// Get external subcommand name
			scName, ok := argOs.toValue()
			if !ok {
				_ = p.resolvePending(matcher)
				return errorInvalidUtf8(p.cmd, outputUsageNew(p.cmd).createUsageWithTitle(nil))
			}

			// Collect the external subcommand args
			scM := parserArgMatcherNew(p.cmd)
			scM.startOccurrenceOfExternal(p.cmd)

			for _, rawVal := range rawArgs.remaining(&argsCursor) {
				val, err := externalParser.parseRef(p.cmd, nil, rawVal, ParserValueSource_CommandLine)
				if err != nil {
					return err
				}
				externalId := Id(idExternal)
				scM.addValTo(externalId, val, rawVal)
			}

			matcher.setSubcommand(&parserSubCommand{
				name:    scName,
				matches: scM.intoInner(),
			})
			return nil
		} else {
			// Start error processing
			_ = p.resolvePending(matcher)
			return p.matchArgError(argOs, validArgFound, trailingValues, matcher)
		}
	}

	if posScName, ok := subcmdName.Get(); ok {
		if p.cmd.IsArgsConflictsWithSubcommandsSet() && validArgFound {
			var others []string
			for _, id := range matcher.argIds() {
				// skip groups
				if a := p.cmd.find(id); a != nil {
					others = append(others, a.String())
				}
			}
			return errorSubcommandConflict(p.cmd, posScName, others,
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
		scName := p.cmd.FindSubcommand(posScName).Expect(internalErrorMsg).GetName()
		if err := p.parseSubcommand(scName, matcher, rawArgs, argsCursor, keepState); err != nil {
			return err
		}
	}

	return nil
}

func (p *parserParser) matchArgError(argOs lexParsedArg, validArgFound bool, trailingValues bool, matcher *parserArgMatcher) *Error {
	// If argument follows a `--`
	if trailingValues {
		// If the arg matches a subcommand name, or any of its aliases (if defined)
		if p.possibleSubcommand(argOs, validArgFound).IsSome() {
			return errorUnnecessaryDoubleDash(p.cmd, argOs.display(),
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
	}

	suggestedTrailingArg := !trailingValues &&
		p.cmd.hasPositionals() &&
		(argOs.isLong() || argOs.isShort())

	if p.cmd.HasSubcommands() {
		if p.cmd.IsArgsConflictsWithSubcommandsSet() && validArgFound {
			var others []string
			for _, id := range matcher.argIds() {
				if a := p.cmd.find(id); a != nil {
					others = append(others, a.String())
				}
			}
			return errorSubcommandConflict(p.cmd, argOs.display(), others,
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}

		candidates := suggestionsDidYouMean(argOs.display(), p.cmd.allSubcommandNames())
		// If the argument looks like a subcommand.
		if len(candidates) != 0 {
			return errorInvalidSubcommand(p.cmd, argOs.display(), candidates,
				p.cmd.getBinNameFallback(), suggestedTrailingArg,
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}

		// If the argument must be a subcommand.
		if !p.cmd.hasPositionals() || p.cmd.isInferSubcommandsSet() {
			return errorUnrecognizedSubcommand(p.cmd, argOs.display(),
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
	}

	return errorUnknownArgument(p.cmd, argOs.display(), rs.None[errorDidYouMean](),
		suggestedTrailingArg, outputUsageNew(p.cmd).createUsageWithTitle(nil))
}

// Checks if the arg matches a subcommand name, or any of its aliases (if defined)
func (p *parserParser) possibleSubcommand(argOs lexParsedArg, validArgFound bool) rs.Option[string] {
	arg, ok := argOs.toValue()
	if !ok {
		return rs.None[string]()
	}

	if !(p.cmd.IsArgsConflictsWithSubcommandsSet() && validArgFound) {
		if p.cmd.isInferSubcommandsSet() {
			// For subcommand `test`, we accepts it's prefix: `t`, `te`,
			// `tes` and `test`.
			var names []string
			for _, s := range p.cmd.GetSubcommands() {
				if strings.HasPrefix(s.GetName(), arg) {
					names = append(names, s.GetName())
					continue
				}

				// Use find here instead of chaining the iterator because we want to accept
				// conflicts in aliases.
				for _, alias := range s.GetAllAliases() {
					if strings.HasPrefix(alias, arg) {
						names = append(names, alias)
						break
					}
				}
			}
			if len(names) == 1 {
				return rs.Some(names[0])
			}
		}

		// Don't use an else here because we want inference to support exact matching even if
		// there are conflicts.
		if sc, ok := p.cmd.FindSubcommand(arg).Get(); ok {
			return rs.Some(sc.GetName())
		}
	}
	return rs.None[string]()
}

// Checks if the arg matches a long flag subcommand name, or any of its aliases (if defined)
func (p *parserParser) possibleLongFlagSubcommand(arg string) rs.Option[string] {
	if p.cmd.isInferSubcommandsSet() {
		var names []string
		for _, sc := range p.cmd.GetSubcommands() {
			long, ok := sc.GetLongFlag().Get()
			if !ok {
				continue
			}
			if strings.HasPrefix(long, arg) {
				names = append(names, sc.GetName())
				continue
			}
			for _, alias := range sc.GetAllLongFlagAliases() {
				if strings.HasPrefix(alias, arg) {
					names = append(names, sc.GetName())
					break
				}
			}
		}
		if len(names) == 1 {
			return rs.Some(names[0])
		}
	}
	if scName, ok := p.cmd.findLongSubcmd(arg).Get(); ok {
		return rs.Some(scName)
	}
	return rs.None[string]()
}

// parseHelpSubcommand always returns an error.
func (p *parserParser) parseHelpSubcommand(cmds []string) *Error {
	cmd := p.cmd.Clone()
	sc := cmd
	for _, name := range cmds {
		if found, ok := sc.FindSubcommand(name).Get(); ok {
			scName := found.GetName()
			sc = sc.buildSubcommand(scName)
			if sc == nil {
				rs.PanicAt(1, "called `Option::unwrap()` on a `None` value")
			}
		} else {
			return errorUnrecognizedSubcommand(sc, toStringLossy(name),
				outputUsageNew(sc).createUsageWithTitle(nil))
		}
	}

	parser := parserParserNew(sc)

	return parser.helpErr(true)
}

func (p *parserParser) isNewArg(next lexParsedArg, currentPositional *Arg) bool {
	current := p.cmd.index(currentPositional.GetId())
	if current.IsAllowHyphenValuesSet() ||
		(current.IsAllowNegativeNumbersSet() && next.isNegativeNumber()) {
		// If allow hyphen, this isn't a new arg.
		return false
	} else if next.isLong() {
		// If this is a long flag, this is a new arg.
		return true
	} else if next.isShort() {
		// If this is a short flag, this is a new arg. But a single '-' by
		// itself is a value and typically means "stdin" on unix systems.
		return true
	}
	// Nothing special, this is a value.
	return false
}

func (p *parserParser) parseSubcommand(scName string, matcher *parserArgMatcher, rawArgs *lexRawArgs, argsCursor lexArgCursor, keepState bool) *Error {
	partialParsingEnabled := p.cmd.isIgnoreErrorsSet()

	if sc := p.cmd.buildSubcommand(scName); sc != nil {
		scMatcher := parserArgMatcherNew(sc)

		{
			sub := parserParserNew(sc)
			// HACK: maintain indexes between parsers
			// FlagSubCommand short arg needs to revisit the current short args, but skip the subcommand itself
			if keepState {
				sub.curIdx = p.curIdx
				sub.flagSubcmdAt = p.flagSubcmdAt
				sub.flagSubcmdSkip = p.flagSubcmdSkip
			}
			if err := sub.getMatchesWith(scMatcher, rawArgs, argsCursor); err != nil {
				if partialParsingEnabled && err.UseStderr() {
				} else {
					return err
				}
			}
		}
		matcher.setSubcommand(&parserSubCommand{
			name:    sc.GetName(),
			matches: scMatcher.intoInner(),
		})
	}
	return nil
}

func (p *parserParser) parseLongArg(matcher *parserArgMatcher, long lexLong, parseState *parserParseState, posCounter int, validArgFound *bool) (parserParseResult, *Error) {
	// maybe here lifetime should be 'a
	if (parseState.kind == parserParseState_Opt || parseState.kind == parserParseState_Pos) &&
		p.cmd.index(parseState.id).IsAllowHyphenValuesSet() {
		return parserParseResult{kind: parserParseResult_MaybeHyphenValue}, nil
	}

	if !long.flagValid {
		return parserParseResult{kind: parserParseResult_NoMatchingArg, arg: toStringLossy(long.flag)}, nil
	}
	longArg := long.flag
	longValue := long.value
	if longArg == "" {
		if rs.DebugAssertions && longValue.IsNone() {
			rs.PanicAt(1, "`--` should be filtered out before this point")
		}
	}

	var arg *Arg
	if found := p.cmd.getKeymap().getLong(longArg); found != nil {
		arg = found
	} else if p.cmd.isInferLongArgsSet() {
		var candidates []*Arg
		for _, a := range p.cmd.GetArguments() {
			if l, ok := a.GetLong().Get(); ok {
				if strings.HasPrefix(l, longArg) {
					candidates = append(candidates, a)
					continue
				}
			}
			for _, alias := range a.aliases {
				if strings.HasPrefix(alias.name, longArg) {
					candidates = append(candidates, a)
					break
				}
			}
		}
		if len(candidates) == 1 {
			arg = candidates[0]
		}
	}

	if arg != nil {
		ident := parserIdentifier_Long
		*validArgFound = true
		if arg.isTakesValueSet() {
			hasEq := longValue.IsSome()
			return p.parseOptValue(ident, longValue, arg, matcher, hasEq)
		} else if rest, ok := longValue.Get(); ok {
			required := p.cmd.requiredGraph()
			var used []Id
			for _, argId := range matcher.argIds() {
				if !matcher.checkExplicit(argId, argPredicateIsPresent) {
					continue
				}
				if a := p.cmd.find(argId); a != nil && (a.IsHideSet() || required.contains(a.GetId())) {
					continue
				}
				used = append(used, argId)
			}
			used = append(used, arg.GetId())
			return parserParseResult{
				kind: parserParseResult_UnneededAttachedValue,
				rest: toStringLossy(rest),
				used: used,
				arg:  arg.String(),
			}, nil
		} else {
			trailingIdx := rs.None[int]()
			return p.react(rs.Some(ident), ParserValueSource_CommandLine, arg, nil, trailingIdx, matcher)
		}
	} else if scName, ok := p.possibleLongFlagSubcommand(longArg).Get(); ok {
		return parserParseResult{kind: parserParseResult_FlagSubCommand, name: scName}, nil
	} else if a := p.cmd.getKeymap().getPosition(posCounter); a != nil && a.IsAllowHyphenValuesSet() && !a.IsLastSet() {
		return parserParseResult{kind: parserParseResult_MaybeHyphenValue}, nil
	} else {
		return parserParseResult{kind: parserParseResult_NoMatchingArg, arg: longArg}, nil
	}
}

func (p *parserParser) parseShortArg(matcher *parserArgMatcher, shortArg lexShortFlags,
	parseState *parserParseState,
	// change this to possible pos_arg when removing the usage of &mut Parser.
	posCounter int, validArgFound *bool) (parserParseResult, *Error) {
	if (parseState.kind == parserParseState_Opt || parseState.kind == parserParseState_Pos) &&
		(p.cmd.index(parseState.id).IsAllowHyphenValuesSet() ||
			(p.cmd.index(parseState.id).IsAllowNegativeNumbersSet() && shortArg.isNegativeNumber())) {
		return parserParseResult{kind: parserParseResult_MaybeHyphenValue}, nil
	} else if a := p.cmd.getKeymap().getPosition(posCounter); a != nil && a.IsAllowNegativeNumbersSet() &&
		shortArg.isNegativeNumber() {
		return parserParseResult{kind: parserParseResult_MaybeHyphenValue}, nil
	} else if a := p.cmd.getKeymap().getPosition(posCounter); a != nil && a.IsAllowHyphenValuesSet() && !a.IsLastSet() &&
		p.anyShortUnknown(shortArg) {
		return parserParseResult{kind: parserParseResult_MaybeHyphenValue}, nil
	}

	ret := parserParseResult{kind: parserParseResult_NoArg}

	skip := p.flagSubcmdSkip
	p.flagSubcmdSkip = 0
	_, advanced := shortArg.advanceBy(skip)
	if rs.DebugAssertions && !advanced {
		rs.PanicAt(1, "tracking of `flag_subcmd_skip` is off for `"+shortArg.inner+"`")
	}
	for {
		c, ok := shortArg.nextFlag().Get()
		if !ok {
			break
		}
		if !c.valid {
			return parserParseResult{kind: parserParseResult_NoMatchingArg, arg: "-" + toStringLossy(c.rest)}, nil
		}
		flag := c.flag

		// Check for matching short options, and return the name if there is no trailing
		// concatenated value: -oval
		// Option: -o
		// Value: val
		if arg := p.cmd.getKeymap().getShort(flag); arg != nil {
			ident := parserIdentifier_Short
			*validArgFound = true
			if !arg.isTakesValueSet() {
				trailingIdx := rs.None[int]()
				result, err := p.react(rs.Some(ident), ParserValueSource_CommandLine, arg, nil, trailingIdx, matcher)
				if err != nil {
					return parserParseResult{}, err
				}
				ret = result
				continue
			}

			// Check for trailing concatenated value
			//
			// Cloning the iterator, so we rollback if it isn't there.
			probe := shortArg
			val := probe.nextValueOs().UnwrapOrDefault()
			valOpt := rs.None[string]()
			if val != "" {
				valOpt = rs.Some(val)
			}

			// Default to "we're expecting a value later".
			//
			// If attached value is not consumed, we may have more short
			// flags to parse, continue.
			//
			// e.g. `-xvf`, when require_equals && x.min_vals == 0, we don't
			// consume the `vf`, even if it's provided as value.
			hasEq := false
			if v, ok := valOpt.Get(); ok {
				if stripped, ok := lexStripPrefix(v, "="); ok {
					valOpt = rs.Some(stripped)
					hasEq = true
				}
			}
			result, err := p.parseOptValue(ident, valOpt, arg, matcher, hasEq)
			if err != nil {
				return parserParseResult{}, err
			}
			if result.kind == parserParseResult_AttachedValueNotConsumed {
				continue
			}
			return result, nil
		}

		if scName, ok := p.cmd.findShortSubcmd(flag).Get(); ok {
			// Make sure indices get updated before reading `self.cur_idx`
			if err := p.resolvePending(matcher); err != nil {
				return parserParseResult{}, err
			}
			p.curIdx++

			name := scName
			// Get the index of the previously saved flag subcommand in the group of flags (if exists).
			// If it is a new flag subcommand, then the formentioned index should be the current one
			// (ie. `cur_idx`), and should be registered.
			curIdx := p.curIdx
			p.flagSubcmdAt.GetOrInsert(curIdx)
			doneShortArgs := shortArg.isEmpty()
			if doneShortArgs {
				p.flagSubcmdAt = rs.None[int]()
			}
			return parserParseResult{kind: parserParseResult_FlagSubCommand, name: name}, nil
		}
		return parserParseResult{kind: parserParseResult_NoMatchingArg, arg: "-" + string(flag)}, nil
	}
	return ret, nil
}

// anyShortUnknown is `short_arg.clone().any(|c| !c.map(|c|
// self.cmd.contains_short(c)).unwrap_or_default())`.
func (p *parserParser) anyShortUnknown(shortArg lexShortFlags) bool {
	probe := shortArg
	for {
		c, ok := probe.nextFlag().Get()
		if !ok {
			return false
		}
		known := c.valid && p.cmd.containsShort(c.flag)
		if !known {
			return true
		}
	}
}

func (p *parserParser) parseOptValue(ident parserIdentifier, attachedValue rs.Option[string], arg *Arg, matcher *parserArgMatcher, hasEq bool) (parserParseResult, *Error) {
	// require_equals is set, but no '=' is provided, try throwing error.
	if arg.IsRequireEqualsSet() && !hasEq {
		if arg.getMinVals() == 0 {
			trailingIdx := rs.None[int]()
			reactResult, err := p.react(rs.Some(ident), ParserValueSource_CommandLine, arg, nil, trailingIdx, matcher)
			if err != nil {
				return parserParseResult{}, err
			}
			if rs.DebugAssertions && reactResult.kind != parserParseResult_ValuesDone {
				rs.PanicAt(1, "assertion `left == right` failed")
			}
			if attachedValue.IsSome() {
				return parserParseResult{kind: parserParseResult_AttachedValueNotConsumed}, nil
			}
			return parserParseResult{kind: parserParseResult_ValuesDone}, nil
		}
		return parserParseResult{kind: parserParseResult_EqualsNotProvided, arg: arg.String()}, nil
	} else if v, ok := attachedValue.Get(); ok {
		argValues := []string{v}
		trailingIdx := rs.None[int]()
		reactResult, err := p.react(rs.Some(ident), ParserValueSource_CommandLine, arg, argValues, trailingIdx, matcher)
		if err != nil {
			return parserParseResult{}, err
		}
		if rs.DebugAssertions && reactResult.kind != parserParseResult_ValuesDone {
			rs.PanicAt(1, "assertion `left == right` failed")
		}
		// Attached are always done
		return parserParseResult{kind: parserParseResult_ValuesDone}, nil
	}
	if err := p.resolvePending(matcher); err != nil {
		return parserParseResult{}, err
	}
	trailingValues := false
	matcher.pendingValuesMut(arg.GetId(), rs.Some(ident), trailingValues)
	return parserParseResult{kind: parserParseResult_Opt, id: arg.GetId()}, nil
}

func (p *parserParser) checkTerminator(arg *Arg, val string) rs.Option[parserParseResult] {
	if terminator, ok := arg.terminator.Get(); ok && val == terminator {
		return rs.Some(parserParseResult{kind: parserParseResult_ValuesDone})
	}
	return rs.None[parserParseResult]()
}

func (p *parserParser) pushArgValues(arg *Arg, rawVals []string, source ParserValueSource, matcher *parserArgMatcher) *Error {
	for _, rawVal := range rawVals {
		// update the current index because each value is a distinct index to clap
		p.curIdx++
		valueParser := arg.GetValueParser()
		val, err := valueParser.parseRef(p.cmd, arg, rawVal, source)
		if err != nil {
			return err
		}

		matcher.addValTo(arg.GetId(), val, rawVal)
		matcher.addIndexTo(arg.GetId(), p.curIdx)
	}

	return nil
}

func (p *parserParser) resolvePending(matcher *parserArgMatcher) *Error {
	pending, ok := matcher.takePending().Get()
	if !ok {
		return nil
	}

	arg := p.cmd.find(pending.id)
	if arg == nil {
		rs.PanicAt(1, internalErrorMsg)
	}
	_, err := p.react(pending.ident, ParserValueSource_CommandLine, arg, pending.rawVals, pending.trailingIdx, matcher)
	return err
}

func (p *parserParser) react(ident rs.Option[parserIdentifier], source ParserValueSource, arg *Arg, rawVals []string, trailingIdx rs.Option[int], matcher *parserArgMatcher) (parserParseResult, *Error) {
	if err := p.resolvePending(matcher); err != nil {
		return parserParseResult{}, err
	}

	// Process before `default_missing_values` to avoid it counting as values from the command
	// line
	if source == ParserValueSource_CommandLine {
		if err := p.verifyNumArgs(arg, rawVals); err != nil {
			return parserParseResult{}, err
		}
	}

	if len(rawVals) == 0 {
		// We assume this case is valid: require equals, but min_vals == 0.
		if len(arg.defaultMissingVals) != 0 {
			trailingIdx = rs.None[int]()
			rawVals = append(rawVals, arg.defaultMissingVals...)
		}
	}

	if valDelim, ok := arg.GetValueDelimiter().Get(); ok {
		if p.cmd.IsDontDelimitTrailingValuesSet() && optionIs(trailingIdx, 0) {
			// Nothing to do
		} else {
			delim := string(valDelim)
			splitRawVals := make([]string, 0, len(rawVals))
			for i, rawVal := range rawVals {
				if !strings.Contains(rawVal, delim) ||
					(p.cmd.IsDontDelimitTrailingValuesSet() && optionIs(trailingIdx, i)) {
					splitRawVals = append(splitRawVals, rawVal)
				} else {
					splitRawVals = append(splitRawVals, strings.Split(rawVal, delim)...)
				}
			}
			rawVals = splitRawVals
		}
	}

	isFlagIdent := false
	if ident, ok := ident.Get(); ok {
		isFlagIdent = ident == parserIdentifier_Short || ident == parserIdentifier_Long
	}

	switch arg.GetAction() {
	case ArgAction_Set:
		if source == ParserValueSource_CommandLine && isFlagIdent {
			// Record flag's index
			p.curIdx++
		}
		if matcher.remove(arg.GetId()) &&
			!(p.cmd.IsArgsOverrideSelf() || idsContain(arg.overrides, arg.GetId())) {
			return parserParseResult{}, errorArgumentConflict(p.cmd, arg.String(), []string{arg.String()},
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
		p.startCustomArg(matcher, arg, source)
		if err := p.pushArgValues(arg, rawVals, source, matcher); err != nil {
			return parserParseResult{}, err
		}
		return parserParseResult{kind: parserParseResult_ValuesDone}, nil
	case ArgAction_Append:
		if source == ParserValueSource_CommandLine && isFlagIdent {
			// Record flag's index
			p.curIdx++
		}
		p.startCustomArg(matcher, arg, source)
		if err := p.pushArgValues(arg, rawVals, source, matcher); err != nil {
			return parserParseResult{}, err
		}
		return parserParseResult{kind: parserParseResult_ValuesDone}, nil
	case ArgAction_SetTrue:
		if len(rawVals) == 0 {
			rawVals = []string{"true"}
		}

		if matcher.remove(arg.GetId()) &&
			!(p.cmd.IsArgsOverrideSelf() || idsContain(arg.overrides, arg.GetId())) {
			return parserParseResult{}, errorArgumentConflict(p.cmd, arg.String(), []string{arg.String()},
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
		p.startCustomArg(matcher, arg, source)
		if err := p.pushArgValues(arg, rawVals, source, matcher); err != nil {
			return parserParseResult{}, err
		}
		return parserParseResult{kind: parserParseResult_ValuesDone}, nil
	case ArgAction_SetFalse:
		if len(rawVals) == 0 {
			rawVals = []string{"false"}
		}

		if matcher.remove(arg.GetId()) &&
			!(p.cmd.IsArgsOverrideSelf() || idsContain(arg.overrides, arg.GetId())) {
			return parserParseResult{}, errorArgumentConflict(p.cmd, arg.String(), []string{arg.String()},
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
		p.startCustomArg(matcher, arg, source)
		if err := p.pushArgValues(arg, rawVals, source, matcher); err != nil {
			return parserParseResult{}, err
		}
		return parserParseResult{kind: parserParseResult_ValuesDone}, nil
	case ArgAction_Count:
		if len(rawVals) == 0 {
			existingValue := ArgMatchesGetOne[builderCountType](&matcher.matches, arg.GetId().AsStr()).UnwrapOr(0)
			nextValue := existingValue
			if nextValue < 255 {
				nextValue++
			}
			rawVals = []string{strconv.FormatUint(uint64(nextValue), 10)}
		}

		matcher.remove(arg.GetId())
		p.startCustomArg(matcher, arg, source)
		if err := p.pushArgValues(arg, rawVals, source, matcher); err != nil {
			return parserParseResult{}, err
		}
		return parserParseResult{kind: parserParseResult_ValuesDone}, nil
	case ArgAction_Help:
		useLong := true
		if ident, ok := ident.Get(); ok && ident == parserIdentifier_Short {
			useLong = false
		}
		return parserParseResult{}, p.helpErr(useLong)
	case ArgAction_HelpShort:
		useLong := false
		return parserParseResult{}, p.helpErr(useLong)
	case ArgAction_HelpLong:
		useLong := true
		return parserParseResult{}, p.helpErr(useLong)
	case ArgAction_Version:
		useLong := true
		if ident, ok := ident.Get(); ok && ident == parserIdentifier_Short {
			useLong = false
		}
		return parserParseResult{}, p.versionErr(useLong)
	default:
		panic(rs.Unreachable(arg.GetAction()))
	}
}

func (p *parserParser) verifyNumArgs(arg *Arg, rawVals []string) *Error {
	if p.cmd.isIgnoreErrorsSet() {
		return nil
	}

	actual := len(rawVals)
	expected := arg.GetNumArgs().Expect(internalErrorMsg)

	if 0 < expected.MinValues() && actual == 0 {
		// Issue 665 (https://github.com/clap-rs/clap/issues/665)
		// Issue 1105 (https://github.com/clap-rs/clap/issues/1105)
		var goodVals []string
		for _, pv := range parserGetPossibleValuesCli(arg) {
			if !pv.IsHideSet() {
				goodVals = append(goodVals, pv.GetName())
			}
		}
		return errorEmptyValue(p.cmd, goodVals, arg.String())
	} else if expectedNum, ok := expected.numValues().Get(); ok {
		if expectedNum != actual {
			return errorWrongNumberOfValues(p.cmd, arg.String(), expectedNum, actual,
				outputUsageNew(p.cmd).createUsageWithTitle(nil))
		}
	} else if actual < expected.MinValues() {
		return errorTooFewValues(p.cmd, arg.String(), expected.MinValues(), actual,
			outputUsageNew(p.cmd).createUsageWithTitle(nil))
	} else if expected.MaxValues() < actual {
		if len(rawVals) == 0 {
			rs.PanicAt(1, internalErrorMsg)
		}
		return errorTooManyValues(p.cmd, toStringLossy(rawVals[len(rawVals)-1]), arg.String(),
			outputUsageNew(p.cmd).createUsageWithTitle(nil))
	}

	return nil
}

func (p *parserParser) removeOverrides(arg *Arg, matcher *parserArgMatcher) {
	for _, overrideId := range arg.overrides {
		matcher.remove(overrideId)
	}

	// Override anything that can override us
	var transitive []Id
	for _, argId := range matcher.argIds() {
		if overrider := p.cmd.find(argId); overrider != nil {
			if idsContain(overrider.overrides, arg.GetId()) {
				transitive = append(transitive, overrider.GetId())
			}
		}
	}
	for _, overriderId := range transitive {
		matcher.remove(overriderId)
	}
}

func (p *parserParser) addEnv(matcher *parserArgMatcher) *Error {
	for _, arg := range p.cmd.GetArguments() {
		// Use env only if the arg was absent among command line args,
		// early return if this is not the case.
		if matcher.contains(arg.id) {
			continue
		}
		if env, ok := arg.env.Get(); ok {
			if val, ok := env.value.Get(); ok {
				argValues := []string{val}
				trailingIdx := rs.None[int]()
				if _, err := p.react(rs.None[parserIdentifier](), ParserValueSource_EnvVariable, arg, argValues, trailingIdx, matcher); err != nil {
					return err
				}
			}
		}
	}

	return nil
}

func (p *parserParser) addDefaults(matcher *parserArgMatcher) *Error {
	for _, arg := range p.cmd.GetArguments() {
		if err := p.addDefaultValue(arg, matcher); err != nil {
			return err
		}
	}

	return nil
}

func (p *parserParser) addDefaultValue(arg *Arg, matcher *parserArgMatcher) *Error {
	if len(arg.defaultValsIfs) != 0 {
		if !matcher.contains(arg.GetId()) {
			for _, ifs := range arg.defaultValsIfs {
				add := false
				if a := matcher.get(ifs.id); a != nil {
					switch val := ifs.predicate.(type) {
					case *BuilderArgPredicate_Equals:
						for _, value := range a.rawValsFlatten() {
							if val.F0 == value {
								add = true
								break
							}
						}
					case *BuilderArgPredicate_IsPresent:
						add = true
					default:
						panic(rs.Unreachable(ifs.predicate))
					}
				}

				if add {
					if defaults, ok := ifs.defaults.Get(); ok {
						argValues := append([]string{}, defaults...)
						trailingIdx := rs.None[int]()
						if _, err := p.react(rs.None[parserIdentifier](), ParserValueSource_DefaultValue, arg, argValues, trailingIdx, matcher); err != nil {
							return err
						}
					}
					return nil
				}
			}
		}
	}

	if len(arg.defaultVals) != 0 {
		if matcher.contains(arg.GetId()) {
			// do nothing
		} else {
			argValues := append([]string{}, arg.defaultVals...)
			trailingIdx := rs.None[int]()
			if _, err := p.react(rs.None[parserIdentifier](), ParserValueSource_DefaultValue, arg, argValues, trailingIdx, matcher); err != nil {
				return err
			}
		}
	}

	return nil
}

func (p *parserParser) startCustomArg(matcher *parserArgMatcher, arg *Arg, source ParserValueSource) {
	if source == ParserValueSource_CommandLine {
		// With each new occurrence, remove overrides from prior occurrences
		p.removeOverrides(arg, matcher)
	}
	matcher.startCustomArg(arg, source)
	if source.isExplicit() {
		for _, group := range p.cmd.groupsForArg(arg.GetId()) {
			matcher.startCustomGroup(group, source)
			matcher.addValTo(group, arg.GetId(), arg.GetId().AsStr())
		}
	}
}

// Error, Help, and Version Methods

func (p *parserParser) didYouMeanError(arg string, matcher *parserArgMatcher, remainingArgs []string, trailingValues bool) *Error {
	// Didn't match a flag or option
	var longs []string
	for i := range p.cmd.getKeymap().keys {
		if key := p.cmd.getKeymap().keys[i].key; key.kind == mkeymapKeyType_Long {
			longs = append(longs, toStringLossy(key.long))
		}
	}

	didYouMean := suggestionsDidYouMeanFlag(arg, remainingArgs, longs, p.cmd.GetSubcommandsMut())

	// Add the arg to the matches to build a proper usage string
	if !p.cmd.isIgnoreErrorsSet() {
		if found, ok := didYouMean.Get(); ok {
			if a := p.cmd.getKeymap().getLong(found.flag); a != nil {
				p.startCustomArg(matcher, a, ParserValueSource_CommandLine)
			}
		}
	}
	didYouMean2 := rs.None[errorDidYouMean]()
	if found, ok := didYouMean.Get(); ok {
		didYouMean2 = rs.Some(errorDidYouMean{flag: "--" + found.flag, sub: found.subcommand})
	}

	required := p.cmd.requiredGraph()
	var used []Id
	for _, argId := range matcher.argIds() {
		if !matcher.checkExplicit(argId, argPredicateIsPresent) {
			continue
		}
		if a := p.cmd.find(argId); a != nil && !a.IsHideSet() {
			used = append(used, argId)
		}
	}

	// `did_you_mean` is a lot more likely and should cause us to skip the `--` suggestion
	// with the one exception being that the CLI is trying to capture arguments
	//
	// In theory, this is only called for `--long`s, so we don't need to check
	anyCapturing := false
	for _, a := range p.cmd.GetPositionals() {
		if a.IsLastSet() || a.IsTrailingVarArgSet() {
			anyCapturing = true
			break
		}
	}
	suggestedTrailingArg := (didYouMean2.IsNone() || anyCapturing) &&
		!trailingValues &&
		p.cmd.hasPositionals()
	return errorUnknownArgument(p.cmd, "--"+arg, didYouMean2, suggestedTrailingArg,
		outputUsageNew(p.cmd).withRequired(required).createUsageWithTitle(used))
}

func (p *parserParser) helpErr(useLong bool) *Error {
	styled := p.cmd.writeHelpErr(useLong)
	return errorDisplayHelp(p.cmd, styled)
}

func (p *parserParser) versionErr(useLong bool) *Error {
	styled := p.cmd.writeVersionErr(useLong)
	return errorDisplayVersion(p.cmd, styled)
}

type parserParseStateKind uint8

const (
	parserParseState_ValuesDone parserParseStateKind = iota
	parserParseState_Opt
	parserParseState_Pos
)

// parserParseState is ParseState: ValuesDone, Opt(Id) or Pos(Id).
type parserParseState struct {
	kind parserParseStateKind
	id   Id
}

type parserParseResultKind uint8

const (
	parserParseResult_FlagSubCommand parserParseResultKind = iota
	parserParseResult_Opt
	parserParseResult_ValuesDone
	parserParseResult_AttachedValueNotConsumed
	parserParseResult_UnneededAttachedValue
	parserParseResult_MaybeHyphenValue
	parserParseResult_EqualsNotProvided
	parserParseResult_NoMatchingArg
	parserParseResult_NoArg
)

// parserParseResult is ParseResult; the fields hold the variant's data:
// FlagSubCommand(name), Opt(id), UnneededAttachedValue { rest, used, arg },
// EqualsNotProvided { arg } and NoMatchingArg { arg }.
type parserParseResult struct {
	kind parserParseResultKind
	name string
	id   Id
	rest string
	used []Id
	arg  string
}

// parserPendingArg is PendingArg.
type parserPendingArg struct {
	id          Id
	ident       rs.Option[parserIdentifier]
	rawVals     []string
	trailingIdx rs.Option[int]
}

// parserIdentifier is Identifier.
type parserIdentifier uint8

const (
	parserIdentifier_Short parserIdentifier = iota
	parserIdentifier_Long
	parserIdentifier_Index
)

var _ = rs.RegisterEnum[parserIdentifier]("Identifier",
	rs.Const(parserIdentifier_Short, "Short"),
	rs.Const(parserIdentifier_Long, "Long"),
	rs.Const(parserIdentifier_Index, "Index"),
)

// optionIs is `option == Some(value)`.
func optionIs[T comparable](option rs.Option[T], value T) bool {
	v, ok := option.Get()
	return ok && v == value
}
