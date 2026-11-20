// Port of clap_builder 4.6.0 src/parser/arg_matcher.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// parserArgMatcher is ArgMatcher.
type parserArgMatcher struct {
	matches ArgMatches
	pending rs.Option[*parserPendingArg]
}

func parserArgMatcherNew(cmd *Command) *parserArgMatcher {
	matcher := &parserArgMatcher{
		pending: rs.None[*parserPendingArg](),
	}
	if debugAssertions {
		for _, a := range cmd.GetArguments() {
			matcher.matches.validArgs = append(matcher.matches.validArgs, a.GetId())
		}
		for _, g := range cmd.GetGroups() {
			matcher.matches.validArgs = append(matcher.matches.validArgs, g.GetId())
		}
		for _, sc := range cmd.GetSubcommands() {
			matcher.matches.validSubcommands = append(matcher.matches.validSubcommands, sc.GetName())
		}
	}
	return matcher
}

func (m *parserArgMatcher) intoInner() *ArgMatches {
	return &m.matches
}

func (m *parserArgMatcher) propagateGlobals(globalArgVec []Id) {
	var valsMap utilFlatMap[Id, *parserMatchedArg]
	m.fillInGlobalValues(globalArgVec, &valsMap)
}

func (m *parserArgMatcher) fillInGlobalValues(globalArgVec []Id, valsMap *utilFlatMap[Id, *parserMatchedArg]) {
	for _, globalArg := range globalArgVec {
		if ma := m.get(globalArg); ma != nil {
			// We have to check if the parent's global arg wasn't used but still exists
			// such as from a default value.
			//
			// For example, `myprog subcommand --global-arg=value` where `--global-arg` defines
			// a default value of `other` myprog would have an existing MatchedArg for
			// `--global-arg` where the value is `other`
			toUpdate := ma
			if parentMa := valsMap.get(globalArg); parentMa != nil {
				if optionSourceGreater((*parentMa).getSource(), ma.getSource()) {
					toUpdate = *parentMa
				} else {
					toUpdate = ma
				}
			}
			valsMap.insert(globalArg, toUpdate.clone())
		}
	}
	if sc := m.matches.subcommand; sc != nil {
		am := &parserArgMatcher{
			matches: *sc.matches,
			pending: rs.None[*parserPendingArg](),
		}
		am.fillInGlobalValues(globalArgVec, valsMap)
		*sc.matches = am.matches
	}

	for i, name := range valsMap.keys {
		m.matches.args.insert(name, valsMap.values[i].clone())
	}
}

// optionSourceGreater is `a > b` for two Option<ValueSource>: None is less
// than any Some.
func optionSourceGreater(a, b rs.Option[ParserValueSource]) bool {
	x, xok := a.Get()
	y, yok := b.Get()
	switch {
	case !xok:
		return false
	case !yok:
		return true
	default:
		return x > y
	}
}

func (m *parserArgMatcher) get(arg Id) *parserMatchedArg {
	if found := m.matches.args.get(arg); found != nil {
		return *found
	}
	return nil
}

func (m *parserArgMatcher) remove(arg Id) bool {
	_, removed := m.matches.args.remove(arg)
	return removed
}

func (m *parserArgMatcher) contains(arg Id) bool {
	return m.matches.args.containsKey(arg)
}

func (m *parserArgMatcher) argIds() []Id {
	return m.matches.args.keys
}

func (m *parserArgMatcher) setSubcommand(sc *parserSubCommand) {
	m.matches.subcommand = sc
}

func (m *parserArgMatcher) subcommandName() rs.Option[string] {
	return m.matches.SubcommandName()
}

func (m *parserArgMatcher) checkExplicit(arg Id, predicate BuilderArgPredicate) bool {
	if a := m.get(arg); a != nil {
		return a.checkExplicit(predicate)
	}
	return false
}

func (m *parserArgMatcher) startCustomArg(arg *Arg, source ParserValueSource) {
	id := arg.GetId()
	ma := *m.matches.args.entryOrInsert(id, parserMatchedArgNewArg(arg))
	if debugAssertions {
		typeId, ok := ma.getTypeId().Get()
		if !ok || !typeId.equal(arg.GetValueParser().TypeId()) {
			rs.PanicAt(1, "assertion `left == right` failed")
		}
	}
	ma.setSource(source)
	ma.newValGroup()
}

func (m *parserArgMatcher) startCustomGroup(id Id, source ParserValueSource) {
	ma := *m.matches.args.entryOrInsert(id, parserMatchedArgNewGroup())
	if debugAssertions && ma.getTypeId().IsSome() {
		rs.PanicAt(1, "assertion `left == right` failed")
	}
	ma.setSource(source)
	ma.newValGroup()
}

func (m *parserArgMatcher) startOccurrenceOfExternal(cmd *Command) {
	id := Id(idExternal)
	ma := *m.matches.args.entryOrInsert(id, parserMatchedArgNewExternal(cmd))
	if debugAssertions {
		typeId, ok := ma.getTypeId().Get()
		if !ok || !typeId.equal(cmd.GetExternalSubcommandValueParser().Expect(internalErrorMsg).TypeId()) {
			rs.PanicAt(1, "assertion `left == right` failed")
		}
	}
	ma.setSource(ParserValueSource_CommandLine)
	ma.newValGroup()
}

func (m *parserArgMatcher) addValTo(arg Id, val any, rawVal string) {
	ma := m.get(arg)
	if ma == nil {
		rs.PanicAt(1, internalErrorMsg)
	}
	ma.appendVal(val, rawVal)
}

func (m *parserArgMatcher) addIndexTo(arg Id, idx int) {
	ma := m.get(arg)
	if ma == nil {
		rs.PanicAt(1, internalErrorMsg)
	}
	ma.pushIndex(idx)
}

func (m *parserArgMatcher) needsMoreVals(o *Arg) bool {
	numPending := 0
	if p, ok := m.pending.Get(); ok && p.id == o.GetId() {
		numPending = len(p.rawVals)
	}
	expected := o.GetNumArgs().Expect(internalErrorMsg)
	return expected.acceptsMore(numPending)
}

func (m *parserArgMatcher) pendingArgId() rs.Option[Id] {
	if p, ok := m.pending.Get(); ok {
		return rs.Some(p.id)
	}
	return rs.None[Id]()
}

// pendingValuesMut returns the pending argument, whose raw_vals the caller
// appends to.
func (m *parserArgMatcher) pendingValuesMut(id Id, ident rs.Option[parserIdentifier], trailingValues bool) *parserPendingArg {
	pending, ok := m.pending.Get()
	if !ok {
		pending = &parserPendingArg{
			id:          id,
			ident:       ident,
			trailingIdx: rs.None[int](),
		}
		m.pending = rs.Some(pending)
	}
	if debugAssertions && pending.id != id {
		rs.PanicAt(1, internalErrorMsg)
	}
	if ident.IsSome() {
		if debugAssertions && pending.ident != ident {
			rs.PanicAt(1, internalErrorMsg)
		}
	}
	if trailingValues {
		if pending.trailingIdx.IsNone() {
			pending.trailingIdx = rs.Some(len(pending.rawVals))
		}
	}
	return pending
}

func (m *parserArgMatcher) startTrailing() {
	if pending, ok := m.pending.Get(); ok {
		// Allow asserting its started on subsequent calls
		if pending.trailingIdx.IsNone() {
			pending.trailingIdx = rs.Some(len(pending.rawVals))
		}
	}
}

func (m *parserArgMatcher) takePending() rs.Option[*parserPendingArg] {
	return m.pending.Take()
}
