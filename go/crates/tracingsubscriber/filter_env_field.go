// Port of tracing-subscriber 0.3.23 src/filter/env/field.rs

package tracingsubscriber

import (
	"math"
	"regexp"
	"regexp/syntax"
	"strings"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// fieldMatch is field::Match: a field name in a directive, with the value it
// must have, if any.
type fieldMatch struct {
	name  string // TODO: allow match patterns for names?
	value rs.Option[*valueMatch]
}

// callsiteMatch is field::CallsiteMatch: the values a span callsite's fields
// must have for a directive to apply.
type callsiteMatch struct {
	fields map[tracing.FieldFieldKey]*valueMatch
	level  tracing.LevelFilter
}

// spanMatch is field::SpanMatch: a callsiteMatch for one span, which
// remembers which of its fields have matched.
type spanMatch struct {
	fields     map[tracing.FieldFieldKey]*spanFieldMatch
	level      tracing.LevelFilter
	hasMatched atomic.Bool
}

type spanFieldMatch struct {
	value   *valueMatch
	matched atomic.Bool
}

// matchVisitor is field::MatchVisitor: it records which fields of a span
// have the values a directive wants.
type matchVisitor struct {
	inner *spanMatch
}

type valueMatchKind uint8

const (
	valueMatchBool valueMatchKind = iota
	valueMatchF64
	valueMatchU64
	valueMatchI64
	valueMatchNaN
	valueMatchDebug
	valueMatchPat
)

// valueMatch is field::ValueMatch.
type valueMatch struct {
	kind valueMatchKind
	b    bool
	f64  float64
	u64  uint64
	i64  int64
	// pattern is the text of a Debug or a Pat match.
	pattern string
	// matcher is the compiled pattern of a Pat match.
	matcher *regexp.Regexp
}

// equal is `impl PartialEq for ValueMatch`. Two Debug matches are never
// equal: the crate's match has no arm for them.
func (v *valueMatch) equal(other *valueMatch) bool {
	if v.kind != other.kind {
		return false
	}
	switch v.kind {
	case valueMatchBool:
		return v.b == other.b
	case valueMatchF64:
		rs.DebugAssert(!math.IsNaN(v.f64))
		rs.DebugAssert(!math.IsNaN(other.f64))
		return v.f64 == other.f64
	case valueMatchU64:
		return v.u64 == other.u64
	case valueMatchI64:
		return v.i64 == other.i64
	case valueMatchNaN:
		return true
	case valueMatchPat:
		return v.pattern == other.pattern
	}
	return false
}

// valueMatchRank orders the kinds the way `impl Ord for ValueMatch` does
// across kinds: Bool < F64 < NaN < U64 < I64 < the patterns.
var valueMatchRank = map[valueMatchKind]int{
	valueMatchBool:  0,
	valueMatchF64:   1,
	valueMatchNaN:   2,
	valueMatchU64:   3,
	valueMatchI64:   4,
	valueMatchPat:   5,
	valueMatchDebug: 5,
}

// compare is `impl Ord for ValueMatch`. Its arms make a Pat greater than a
// Debug and a Debug greater than a Pat; both are kept.
func (v *valueMatch) compare(other *valueMatch) int {
	if v.kind != other.kind {
		if v.kind == valueMatchPat || v.kind == valueMatchDebug {
			return 1
		}
		return cmpInt(valueMatchRank[v.kind], valueMatchRank[other.kind])
	}
	switch v.kind {
	case valueMatchBool:
		switch {
		case v.b == other.b:
			return 0
		case !v.b:
			return -1
		}
		return 1
	case valueMatchF64:
		if math.IsNaN(v.f64) || math.IsNaN(other.f64) {
			panic(rs.Panicf("`ValueMatch::F64` may not contain `NaN` values"))
		}
		switch {
		case v.f64 < other.f64:
			return -1
		case v.f64 > other.f64:
			return 1
		}
		return 0
	case valueMatchNaN:
		return 0
	case valueMatchU64:
		switch {
		case v.u64 < other.u64:
			return -1
		case v.u64 > other.u64:
			return 1
		}
		return 0
	case valueMatchI64:
		switch {
		case v.i64 < other.i64:
			return -1
		case v.i64 > other.i64:
			return 1
		}
		return 0
	}
	return strings.Compare(v.pattern, other.pattern)
}

// ===== impl Match =====

func (m *fieldMatch) hasValue() bool {
	return m.value.IsSome()
}

func (m *fieldMatch) nameMethod() string {
	return m.name
}

// fieldMatchParse is Match::parse: `name` or `name=value`. A value is a
// bool, an integer, a float, or else a regular expression (or with regex
// off, the Debug text the value must start).
func fieldMatchParse(s string, regex bool) (*fieldMatch, error) {
	parts := strings.Split(s, "=")
	name := parts[0]
	value := rs.None[*valueMatch]()
	if len(parts) > 1 {
		part := parts[1]
		if regex {
			parsed, err := valueMatchParseRegex(part)
			if err != nil {
				return nil, err
			}
			value = rs.Some(parsed)
		} else {
			value = rs.Some(valueMatchParseNonRegex(part))
		}
	}
	return &fieldMatch{name: name, value: value}, nil
}

// String is `impl Display for Match`.
func (m *fieldMatch) String() string {
	if value, ok := m.value.Get(); ok {
		return m.name + "=" + value.String()
	}
	return m.name
}

// equal is the derived PartialEq of Match.
func (m *fieldMatch) equal(other *fieldMatch) bool {
	if m.name != other.name {
		return false
	}
	a, aOk := m.value.Get()
	b, bOk := other.value.Get()
	if aOk != bOk {
		return false
	}
	return !aOk || a.equal(b)
}

// compare is `impl Ord for Match`.
func (m *fieldMatch) compare(other *fieldMatch) int {
	hasValue := 0
	switch {
	case m.value.IsSome() && other.value.IsNone():
		hasValue = 1
	case m.value.IsNone() && other.value.IsSome():
		hasValue = -1
	}
	if hasValue != 0 {
		return hasValue
	}
	if ordering := strings.Compare(m.name, other.name); ordering != 0 {
		return ordering
	}
	a, aOk := m.value.Get()
	b, bOk := other.value.Get()
	switch {
	case !aOk && !bOk:
		return 0
	case !aOk:
		return -1
	case !bOk:
		return 1
	}
	return a.compare(b)
}

func valueMatchFromF64(v float64) *valueMatch {
	if math.IsNaN(v) {
		return &valueMatch{kind: valueMatchNaN}
	}
	return &valueMatch{kind: valueMatchF64, f64: v}
}

func valueMatchParseRegex(s string) (*valueMatch, error) {
	if b, err := strs.ParseBool(s); err == nil {
		return &valueMatch{kind: valueMatchBool, b: b}, nil
	}
	if u, err := strs.ParseU64(s); err == nil {
		return &valueMatch{kind: valueMatchU64, u64: u}, nil
	}
	if i, err := strs.ParseI64(s); err == nil {
		return &valueMatch{kind: valueMatchI64, i64: i}, nil
	}
	if f, err := strs.ParseF64(s); err == nil {
		return valueMatchFromF64(f), nil
	}
	pattern, err := matchPatternFromStr(s)
	if err != nil {
		return nil, err
	}
	return pattern, nil
}

func valueMatchParseNonRegex(s string) *valueMatch {
	if b, err := strs.ParseBool(s); err == nil {
		return &valueMatch{kind: valueMatchBool, b: b}
	}
	if u, err := strs.ParseU64(s); err == nil {
		return &valueMatch{kind: valueMatchU64, u64: u}
	}
	if i, err := strs.ParseI64(s); err == nil {
		return &valueMatch{kind: valueMatchI64, i64: i}
	}
	if f, err := strs.ParseF64(s); err == nil {
		return valueMatchFromF64(f)
	}
	return &valueMatch{kind: valueMatchDebug, pattern: s}
}

// String is `impl Display for ValueMatch`.
func (v *valueMatch) String() string {
	switch v.kind {
	case valueMatchBool:
		return rs.ToString(v.b)
	case valueMatchF64:
		return rs.ToString(v.f64)
	case valueMatchNaN:
		return rs.ToString(math.NaN())
	case valueMatchI64:
		return rs.ToString(v.i64)
	case valueMatchU64:
		return rs.ToString(v.u64)
	}
	return v.pattern
}

// ===== MatchPattern =====

// regexBuildError is matchers::BuildError (regex_automata's dense DFA build
// error), as its Display prints.
type regexBuildError struct {
	message string
}

func (e *regexBuildError) Error() string { return e.message }

// matchPatternFromStr is `impl FromStr for MatchPattern`.
//
// The crate builds a regex_automata DFA. Go's regexp has the same syntax and
// the same leftmost-first semantics for the patterns a filter is written
// with; it differs where Rust's classes are Unicode and Go's are ASCII (\d,
// \w, \s), and it rejects Rust-only syntax such as `(?-u)`, which the crate
// accepts. A pattern Go cannot parse is the crate's "error building NFA",
// and a word boundary, which the crate's DFA does not support in Unicode
// mode, is the crate's unsupported-feature error.
func matchPatternFromStr(s string) (*valueMatch, error) {
	parsed, err := syntax.Parse(s, syntax.Perl)
	if err != nil {
		return nil, &regexBuildError{message: "error building NFA"}
	}
	if hasWordBoundary(parsed) {
		return nil, &regexBuildError{message: "unsupported regex feature for DFAs: " +
			"cannot build DFAs for regexes with Unicode word boundaries; switch to ASCII word " +
			"boundaries, or heuristically enable Unicode word boundaries or use a different regex engine"}
	}
	matcher, err := regexp.Compile(s)
	if err != nil {
		return nil, &regexBuildError{message: "error building NFA"}
	}
	return &valueMatch{kind: valueMatchPat, pattern: s, matcher: matcher}, nil
}

func hasWordBoundary(re *syntax.Regexp) bool {
	if re.Op == syntax.OpWordBoundary || re.Op == syntax.OpNoWordBoundary {
		return true
	}
	for _, sub := range re.Sub {
		if hasWordBoundary(sub) {
			return true
		}
	}
	return false
}

// strMatches is MatchPattern::str_matches: the DFA, started unanchored and
// fed the whole text, is in a match state at the end. That is: the
// leftmost-first match exists and ends where the text ends.
func (v *valueMatch) strMatches(s string) bool {
	loc := v.matcher.FindStringIndex(s)
	return loc != nil && loc[1] == len(s)
}

// patternDebugMatches is MatchPattern::debug_matches: strMatches on the
// value's Debug text.
func (v *valueMatch) patternDebugMatches(value any) bool {
	return v.strMatches(tracing.FieldDebugString(value))
}

// intoDebugMatch is MatchPattern::into_debug_match.
func (v *valueMatch) intoDebugMatch() *valueMatch {
	return &valueMatch{kind: valueMatchDebug, pattern: v.pattern}
}

// debugMatches is MatchDebug::debug_matches. The value's Debug text is
// written into a writer that accepts each piece only while the pattern
// starts with it, and the match is whether every piece was accepted: the
// text must begin the pattern, not equal it.
func (v *valueMatch) debugMatches(value any) bool {
	return strings.HasPrefix(v.pattern, tracing.FieldDebugString(value))
}

// ===== impl CallsiteMatch =====

func (m *callsiteMatch) toSpanMatch() *spanMatch {
	fields := make(map[tracing.FieldFieldKey]*spanFieldMatch, len(m.fields))
	for k, v := range m.fields {
		fields[k] = &spanFieldMatch{value: v}
	}
	return &spanMatch{fields: fields, level: m.level}
}

// ===== impl SpanMatch =====

func (m *spanMatch) visitor() *matchVisitor {
	return &matchVisitor{inner: m}
}

func (m *spanMatch) isMatched() bool {
	if m.hasMatched.Load() {
		return true
	}
	return m.isMatchedSlow()
}

func (m *spanMatch) isMatchedSlow() bool {
	matched := true
	for _, field := range m.fields {
		if !field.matched.Load() {
			matched = false
			break
		}
	}
	if matched {
		m.hasMatched.Store(true)
	}
	return matched
}

func (m *spanMatch) filter() rs.Option[tracing.LevelFilter] {
	if m.isMatched() {
		return rs.Some(m.level)
	}
	return rs.None[tracing.LevelFilter]()
}

// ===== impl Visit for MatchVisitor =====

func (v *matchVisitor) RecordF64(field *tracing.FieldField, value float64) {
	if m, ok := v.inner.fields[field.Key()]; ok {
		switch {
		case m.value.kind == valueMatchNaN && math.IsNaN(value):
			m.matched.Store(true)
		case m.value.kind == valueMatchF64 && math.Abs(value-m.value.f64) < epsilonF64:
			m.matched.Store(true)
		}
	}
}

// epsilonF64 is f64::EPSILON.
const epsilonF64 = 2.220446049250313e-16

func (v *matchVisitor) RecordI64(field *tracing.FieldField, value int64) {
	if m, ok := v.inner.fields[field.Key()]; ok {
		switch {
		case m.value.kind == valueMatchI64 && value == m.value.i64:
			m.matched.Store(true)
		case m.value.kind == valueMatchU64 && value >= 0 && uint64(value) == m.value.u64:
			m.matched.Store(true)
		}
	}
}

func (v *matchVisitor) RecordU64(field *tracing.FieldField, value uint64) {
	if m, ok := v.inner.fields[field.Key()]; ok {
		if m.value.kind == valueMatchU64 && value == m.value.u64 {
			m.matched.Store(true)
		}
	}
}

func (v *matchVisitor) RecordBool(field *tracing.FieldField, value bool) {
	if m, ok := v.inner.fields[field.Key()]; ok {
		if m.value.kind == valueMatchBool && value == m.value.b {
			m.matched.Store(true)
		}
	}
}

func (v *matchVisitor) RecordStr(field *tracing.FieldField, value string) {
	if m, ok := v.inner.fields[field.Key()]; ok {
		switch {
		case m.value.kind == valueMatchPat && m.value.strMatches(value):
			m.matched.Store(true)
		case m.value.kind == valueMatchDebug && m.value.debugMatches(value):
			m.matched.Store(true)
		}
	}
}

func (v *matchVisitor) RecordDebug(field *tracing.FieldField, value any) {
	if m, ok := v.inner.fields[field.Key()]; ok {
		switch {
		case m.value.kind == valueMatchPat && m.value.patternDebugMatches(value):
			m.matched.Store(true)
		case m.value.kind == valueMatchDebug && m.value.debugMatches(value):
			m.matched.Store(true)
		}
	}
}

func (v *matchVisitor) RecordBytes(field *tracing.FieldField, value []byte) {
	tracing.FieldVisitDefaultRecordBytes(v, field, value)
}

func (v *matchVisitor) RecordError(field *tracing.FieldField, value error) {
	tracing.FieldVisitDefaultRecordError(v, field, value)
}
