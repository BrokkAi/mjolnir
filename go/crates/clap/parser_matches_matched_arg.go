// Port of clap_builder 4.6.0 src/parser/matches/matched_arg.rs

package clap

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// parserMatchedArg is MatchedArg.
//
// A value (AnyValue in Rust) is a Go `any`.
type parserMatchedArg struct {
	source     rs.Option[ParserValueSource]
	indices    []int
	typeId     rs.Option[utilAnyValueId]
	vals       [][]any
	rawVals    [][]string
	ignoreCase bool
}

func parserMatchedArgNewArg(arg *Arg) *parserMatchedArg {
	ignoreCase := arg.IsIgnoreCaseSet()
	return &parserMatchedArg{
		source:     rs.None[ParserValueSource](),
		typeId:     rs.Some(arg.GetValueParser().TypeId()),
		ignoreCase: ignoreCase,
	}
}

func parserMatchedArgNewGroup() *parserMatchedArg {
	ignoreCase := false
	return &parserMatchedArg{
		source:     rs.None[ParserValueSource](),
		typeId:     rs.None[utilAnyValueId](),
		ignoreCase: ignoreCase,
	}
}

func parserMatchedArgNewExternal(cmd *Command) *parserMatchedArg {
	ignoreCase := false
	return &parserMatchedArg{
		source:     rs.None[ParserValueSource](),
		typeId:     rs.Some(cmd.GetExternalSubcommandValueParser().Expect(internalErrorMsg).TypeId()),
		ignoreCase: ignoreCase,
	}
}

func (m *parserMatchedArg) getIndex(index int) rs.Option[int] {
	if index < len(m.indices) {
		return rs.Some(m.indices[index])
	}
	return rs.None[int]()
}

func (m *parserMatchedArg) pushIndex(index int) {
	m.indices = append(m.indices, index)
}

func (m *parserMatchedArg) valsFlatten() []any {
	var flat []any
	for _, group := range m.vals {
		flat = append(flat, group...)
	}
	return flat
}

func (m *parserMatchedArg) rawValsFlatten() []string {
	var flat []string
	for _, group := range m.rawVals {
		flat = append(flat, group...)
	}
	return flat
}

func (m *parserMatchedArg) first() (any, bool) {
	for _, group := range m.vals {
		if len(group) > 0 {
			return group[0], true
		}
	}
	return nil, false
}

func (m *parserMatchedArg) firstRaw() rs.Option[string] {
	for _, group := range m.rawVals {
		if len(group) > 0 {
			return rs.Some(group[0])
		}
	}
	return rs.None[string]()
}

func (m *parserMatchedArg) newValGroup() {
	m.vals = append(m.vals, []any{})
	m.rawVals = append(m.rawVals, []string{})
}

func (m *parserMatchedArg) appendVal(val any, rawVal string) {
	// We assume there is always a group created before.
	if len(m.vals) == 0 || len(m.rawVals) == 0 {
		rs.PanicAt(1, internalErrorMsg)
	}
	m.vals[len(m.vals)-1] = append(m.vals[len(m.vals)-1], val)
	m.rawVals[len(m.rawVals)-1] = append(m.rawVals[len(m.rawVals)-1], rawVal)
}

func (m *parserMatchedArg) numVals() int {
	total := 0
	for _, v := range m.vals {
		total += len(v)
	}
	return total
}

func (m *parserMatchedArg) checkExplicit(predicate BuilderArgPredicate) bool {
	if source, ok := m.source.Get(); ok && !source.isExplicit() {
		return false
	}

	switch predicate := predicate.(type) {
	case *BuilderArgPredicate_Equals:
		val := predicate.F0
		for _, v := range m.rawValsFlatten() {
			if m.ignoreCase {
				// If `v` isn't utf8, it can't match `val`, so `OsStr::to_str` should be fine
				if utilEqIgnoreCase(toStringLossy(v), toStringLossy(val)) {
					return true
				}
			} else if v == val {
				return true
			}
		}
		return false
	case *BuilderArgPredicate_IsPresent:
		return true
	default:
		panic(rs.Unreachable())
	}
}

func (m *parserMatchedArg) getSource() rs.Option[ParserValueSource] {
	return m.source
}

func (m *parserMatchedArg) setSource(source ParserValueSource) {
	if existing, ok := m.source.Get(); ok {
		m.source = rs.Some(max(existing, source))
	} else {
		m.source = rs.Some(source)
	}
}

func (m *parserMatchedArg) getTypeId() rs.Option[utilAnyValueId] {
	return m.typeId
}

func (m *parserMatchedArg) inferTypeId(expected utilAnyValueId) utilAnyValueId {
	if typeId, ok := m.getTypeId().Get(); ok {
		return typeId
	}
	for _, v := range m.valsFlatten() {
		actual := utilAnyValueIdOfValue(v)
		if !actual.equal(expected) {
			return actual
		}
	}
	return expected
}

// clone is the derived Clone of MatchedArg. The values themselves are
// shared, as cloning an AnyValue shares its Arc.
func (m *parserMatchedArg) clone() *parserMatchedArg {
	cloned := &parserMatchedArg{
		source:     m.source,
		indices:    append([]int(nil), m.indices...),
		typeId:     m.typeId,
		ignoreCase: m.ignoreCase,
	}
	cloned.vals = make([][]any, len(m.vals))
	for i, group := range m.vals {
		cloned.vals[i] = append([]any{}, group...)
	}
	cloned.rawVals = make([][]string, len(m.rawVals))
	for i, group := range m.rawVals {
		cloned.rawVals[i] = append([]string{}, group...)
	}
	return cloned
}

// equal is `impl PartialEq for MatchedArg`: everything but the parsed
// values.
func (m *parserMatchedArg) equal(other *parserMatchedArg) bool {
	if m.source != other.source || m.ignoreCase != other.ignoreCase {
		return false
	}
	if len(m.indices) != len(other.indices) {
		return false
	}
	for i := range m.indices {
		if m.indices[i] != other.indices[i] {
			return false
		}
	}
	a, aok := m.typeId.Get()
	b, bok := other.typeId.Get()
	if aok != bok || (aok && !a.equal(b)) {
		return false
	}
	if len(m.rawVals) != len(other.rawVals) {
		return false
	}
	for i := range m.rawVals {
		if len(m.rawVals[i]) != len(other.rawVals[i]) {
			return false
		}
		for j := range m.rawVals[i] {
			if m.rawVals[i][j] != other.rawVals[i][j] {
				return false
			}
		}
	}
	return true
}

// utilAnyValueIdOfValue is AnyValue::type_id: the type of a stored value.
func utilAnyValueIdOfValue(value any) utilAnyValueId {
	t := reflect.TypeOf(value)
	if t == nil {
		return utilAnyValueId{typeName: "nil"}
	}
	return utilAnyValueId{typeId: t, typeName: rustTypeName(t)}
}
