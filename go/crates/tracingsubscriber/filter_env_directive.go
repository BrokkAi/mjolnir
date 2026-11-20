// Port of tracing-subscriber 0.3.23 src/filter/env/directive.rs

package tracingsubscriber

import (
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// FilterDirective is tracing_subscriber::filter::Directive: one directive of
// an EnvFilter, `target[span{field=value}]=level` with every part optional.
type FilterDirective struct {
	inSpan rs.Option[string]
	fields []*fieldMatch
	target rs.Option[string]
	level  tracing.LevelFilter
}

// matchSet is MatchSet: the matches of the dynamic directives for one
// callsite (CallsiteMatcher) or one span (SpanMatcher).
type matchSet[T any] struct {
	fieldMatches []T
	baseLevel    tracing.LevelFilter
}

type callsiteMatcher = matchSet[*callsiteMatch]

type spanMatcher = matchSet[*spanMatch]

func (d *FilterDirective) hasName() bool {
	return d.inSpan.IsSome()
}

func (d *FilterDirective) hasFields() bool {
	return len(d.fields) != 0
}

// toStatic is Directive::to_static.
func (d *FilterDirective) toStatic() rs.Option[*staticDirective] {
	if !d.isStatic() {
		return rs.None[*staticDirective]()
	}

	// TODO(eliza): these strings are all immutable; we should consider
	// `Arc`ing them to make this more efficient...
	fieldNames := make([]string, 0, len(d.fields))
	for _, field := range d.fields {
		fieldNames = append(fieldNames, field.nameMethod())
	}

	return rs.Some(staticDirectiveNew(d.target, fieldNames, d.level))
}

func (d *FilterDirective) isStatic() bool {
	if d.hasName() {
		return false
	}
	for _, field := range d.fields {
		if field.hasValue() {
			return false
		}
	}
	return true
}

func (d *FilterDirective) isDynamic() bool {
	return d.hasName() || d.hasFields()
}

// fieldMatcher is Directive::field_matcher: the values the callsite's fields
// must have, or None when the callsite lacks a field the directive names.
func (d *FilterDirective) fieldMatcher(meta *tracing.Metadata) rs.Option[*callsiteMatch] {
	fieldset := meta.Fields()
	fields := make(map[tracing.FieldFieldKey]*valueMatch)
	for _, m := range d.fields {
		field, ok := fieldset.Field(m.name).Get()
		if !ok {
			return rs.None[*callsiteMatch]()
		}
		value, ok := m.value.Get()
		if !ok {
			continue
		}
		fields[field.Key()] = value
	}
	return rs.Some(&callsiteMatch{fields: fields, level: d.level})
}

// directiveMakeTables is Directive::make_tables: the dynamic directives, and
// the static ones, which include the static form of each dynamic one.
func directiveMakeTables(directives []*FilterDirective) (directiveSet[*FilterDirective], directiveSet[*staticDirective]) {
	var dyns, stats []*FilterDirective
	for _, d := range directives {
		if d.isDynamic() {
			dyns = append(dyns, d)
		} else {
			stats = append(stats, d)
		}
	}
	var statics []*staticDirective
	for _, d := range stats {
		if s, ok := d.toStatic().Get(); ok {
			statics = append(statics, s)
		}
	}
	for _, d := range dyns {
		if s, ok := d.toStatic().Get(); ok {
			statics = append(statics, s)
		}
	}
	return directiveSetFrom(dyns), directiveSetFrom(statics)
}

// deregexify is Directive::deregexify: every pattern becomes a Debug match.
func (d *FilterDirective) deregexify() {
	for _, field := range d.fields {
		if value, ok := field.value.Get(); ok && value.kind == valueMatchPat {
			field.value = rs.Some(value.intoDebugMatch())
		}
	}
}

type parseState uint8

const (
	parseStart parseState = iota
	parseLevelOrTarget
	parseSpan
	parseField
	parseFields
	parseTarget
	parseLevel
	parseComplete
)

// directiveParse is Directive::parse.
//
// It walks the characters of the trimmed text but cuts the untrimmed text at
// their positions, as the crate does, so leading whitespace shifts every cut
// (and can make one fall inside a character, which panics as Rust's string
// slicing does).
func directiveParse(from string, regex bool) (*FilterDirective, error) {
	cur := &FilterDirective{
		level:  tracing.LevelFilterTrace,
		target: rs.None[string](),
		inSpan: rs.None[string](),
		fields: nil,
	}

	state := parseStart
	start := 0
	trimmed := strs.Trim(from)
	for i, c := range trimmed {
		switch {
		case state == parseStart && c == '[':
			state, start = parseSpan, i+1
		case state == parseStart && c != '-' && c != ':' && c != '_' && !strs.IsAlphanumeric(c):
			return nil, filterParseErrorNew()
		case state == parseStart:
			state, start = parseLevelOrTarget, i
		case state == parseLevelOrTarget && c == '=':
			cur.target = rs.Some(strs.Slice(from, start, i))
			state, start = parseLevel, i+1
		case state == parseLevelOrTarget && c == '[':
			cur.target = rs.Some(strs.Slice(from, start, i))
			state, start = parseSpan, i+1
		case state == parseLevelOrTarget && c == ',':
			cur.level, cur.target = levelOrTarget(strs.SliceFrom(from, start))
			state = parseComplete
		case state == parseLevelOrTarget:
		case state == parseTarget && c == '=':
			state, start = parseLevel, i+1
		case state == parseSpan && c == ']':
			cur.inSpan = rs.Some(strs.Slice(from, start, i))
			state = parseTarget
		case state == parseSpan && c == '{':
			if name := strs.Slice(from, start, i); name == "" {
				cur.inSpan = rs.None[string]()
			} else {
				cur.inSpan = rs.Some(name)
			}
			state, start = parseField, i+1
		case state == parseSpan:
		case state == parseField && (c == '}' || c == ','):
			field := strs.Slice(from, start, i)
			if field == "" {
				return nil, filterParseErrorNew()
			}
			parsed, err := fieldMatchParse(field, regex)
			if err != nil {
				return nil, filterParseErrorFromField(err)
			}
			cur.fields = append(cur.fields, parsed)
			if c == '}' {
				state = parseFields
			} else {
				state, start = parseField, i+1
			}
		case state == parseField:
		case state == parseFields && c == ']':
			state = parseTarget
		case state == parseLevel && c == ',':
			level, err := directiveLevel(strs.Slice(from, start, i))
			if err != nil {
				return nil, err
			}
			cur.level = level
			state = parseComplete
		case state == parseLevel:
		default:
			return nil, filterParseErrorNew()
		}
	}

	switch state {
	case parseLevelOrTarget:
		cur.level, cur.target = levelOrTarget(strs.SliceFrom(from, start))
	case parseLevel:
		level, err := directiveLevel(strs.SliceFrom(from, start))
		if err != nil {
			return nil, err
		}
		cur.level = level
	case parseTarget, parseComplete:
	default:
		return nil, filterParseErrorNew()
	}

	return cur, nil
}

// levelOrTarget reads the text of a directive that is a level alone or a
// target alone.
func levelOrTarget(text string) (tracing.LevelFilter, rs.Option[string]) {
	if text == "" {
		return tracing.LevelFilterTrace, rs.None[string]()
	}
	if level, err := tracing.LevelFilterFromStr(text); err == nil {
		return level, rs.None[string]()
	}
	return tracing.LevelFilterTrace, rs.Some(text)
}

// directiveLevel reads the level after `=`: empty is trace.
func directiveLevel(text string) (tracing.LevelFilter, error) {
	if text == "" {
		return tracing.LevelFilterTrace, nil
	}
	level, err := tracing.LevelFilterFromStr(text)
	if err != nil {
		return 0, filterParseErrorFromLevel(err.(*tracing.ParseLevelFilterError))
	}
	return level, nil
}

// caresAbout is `impl Match for Directive`.
func (d *FilterDirective) caresAbout(meta *tracing.Metadata) bool {
	// Does this directive have a target filter, and does it match the
	// metadata's target?
	if target, ok := d.target.Get(); ok {
		if !strings.HasPrefix(meta.Target(), target) {
			return false
		}
	}

	// Do we have a name filter, and does it match the metadata's name?
	// TODO(eliza): put name globbing here?
	if name, ok := d.inSpan.Get(); ok {
		if name != meta.Name() {
			return false
		}
	}

	// Does the metadata define all the fields that this directive cares about?
	actualFields := meta.Fields()
	for _, expectedField := range d.fields {
		// Does the actual field set (from the metadata) contain this field?
		if actualFields.Field(expectedField.name).IsNone() {
			return false
		}
	}

	return true
}

func (d *FilterDirective) matchLevel() tracing.LevelFilter { return d.level }

// FilterDirectiveFromStr is `impl FromStr for Directive`: Directive::parse
// with regular expressions.
func FilterDirectiveFromStr(from string) (*FilterDirective, error) {
	return directiveParse(from, true)
}

// FilterDirectiveDefault is `impl Default for Directive`: off for
// everything.
func FilterDirectiveDefault() *FilterDirective {
	return &FilterDirective{
		level:  tracing.LevelFilterOff,
		target: rs.None[string](),
		inSpan: rs.None[string](),
		fields: nil,
	}
}

// compare is `impl Ord for Directive`: the most specific directive is the
// least.
func (d *FilterDirective) compare(other *FilterDirective) int {
	// We attempt to order directives by how "specific" they are. This
	// ensures that we try the most specific directives first when
	// attempting to match a piece of metadata.

	// First, we compare based on whether a target is specified, and the
	// lengths of those targets if both have targets.
	ordering := cmpOptionLen(d.target, other.target)
	// Next compare based on the presence of span names.
	if ordering == 0 {
		ordering = cmpBool(d.inSpan.IsSome(), other.inSpan.IsSome())
	}
	// Then we compare how many fields are defined by each
	// directive.
	if ordering == 0 {
		ordering = cmpInt(len(d.fields), len(other.fields))
	}
	// Finally, we fall back to lexicographical ordering if the directives are
	// equally specific. Although this is no longer semantically important,
	// we need to provide a total ordering to determine the directive's place
	// in the BTreeMap.
	if ordering == 0 {
		ordering = cmpOptionString(d.target, other.target)
		if ordering == 0 {
			ordering = cmpOptionString(d.inSpan, other.inSpan)
		}
		if ordering == 0 {
			ordering = slices.CompareFunc(d.fields, other.fields, (*fieldMatch).compare)
		}
	}
	// Now that we've ordered the directives by specificity, reverse the
	// ordering so that the most specific directive is the least.
	ordering = -ordering

	if rs.DebugAssertions && ordering == 0 {
		rs.DebugAssert(d.target == other.target,
			"invariant violated: Ordering::Equal must imply a.target == b.target")
		rs.DebugAssert(d.inSpan == other.inSpan,
			"invariant violated: Ordering::Equal must imply a.in_span == b.in_span")
		rs.DebugAssert(slices.EqualFunc(d.fields, other.fields, (*fieldMatch).equal),
			"invariant violated: Ordering::Equal must imply a.fields == b.fields")
	}

	return ordering
}

func cmpBool(a, b bool) int {
	switch {
	case a == b:
		return 0
	case !a:
		return -1
	}
	return 1
}

// Equal is the derived PartialEq of Directive.
func (d *FilterDirective) Equal(other *FilterDirective) bool {
	return d.inSpan == other.inSpan &&
		slices.EqualFunc(d.fields, other.fields, (*fieldMatch).equal) &&
		d.target == other.target &&
		d.level == other.level
}

// String is `impl Display for Directive`.
func (d *FilterDirective) String() string {
	var f strings.Builder
	wroteAny := false
	if target, ok := d.target.Get(); ok {
		f.WriteString(target)
		wroteAny = true
	}

	if d.inSpan.IsSome() || len(d.fields) != 0 {
		f.WriteString("[")

		if span, ok := d.inSpan.Get(); ok {
			f.WriteString(span)
		}

		if len(d.fields) != 0 {
			f.WriteString("{")
			for i, field := range d.fields {
				if i > 0 {
					f.WriteString(",")
				}
				f.WriteString(field.String())
			}
			f.WriteString("}")
		}

		f.WriteString("]")
		wroteAny = true
	}

	if wroteAny {
		f.WriteString("=")
	}

	f.WriteString(d.level.String())
	return f.String()
}

// FmtDisplay is `impl Display for Directive`. The target, the span name and
// the level are written with the formatter's flags, the rest as it is.
func (d *FilterDirective) FmtDisplay(f *rs.Formatter) error {
	wroteAny := false
	if target, ok := d.target.Get(); ok {
		if err := rs.DisplayFmt(target, f); err != nil {
			return err
		}
		wroteAny = true
	}

	if d.inSpan.IsSome() || len(d.fields) != 0 {
		if err := f.WriteStr("["); err != nil {
			return err
		}

		if span, ok := d.inSpan.Get(); ok {
			if err := rs.DisplayFmt(span, f); err != nil {
				return err
			}
		}

		if len(d.fields) != 0 {
			var fields strings.Builder
			fields.WriteString("{")
			for i, field := range d.fields {
				if i > 0 {
					fields.WriteString(",")
				}
				fields.WriteString(field.String())
			}
			fields.WriteString("}")
			if err := f.WriteStr(fields.String()); err != nil {
				return err
			}
		}

		if err := f.WriteStr("]"); err != nil {
			return err
		}
		wroteAny = true
	}

	if wroteAny {
		if err := f.WriteStr("="); err != nil {
			return err
		}
	}

	return d.level.FmtDisplay(f)
}

// FilterDirectiveFromLevelFilter is `impl From<LevelFilter> for Directive`.
func FilterDirectiveFromLevelFilter(level tracing.LevelFilter) *FilterDirective {
	directive := FilterDirectiveDefault()
	directive.level = level
	return directive
}

// FilterDirectiveFromLevel is `impl From<Level> for Directive`.
func FilterDirectiveFromLevel(level tracing.Level) *FilterDirective {
	return FilterDirectiveFromLevelFilter(tracing.LevelFilterFromLevel(level))
}

// clone is the derived Clone of Directive: the field matches are shared
// until deregexify replaces one, so their list is copied.
func (d *FilterDirective) clone() *FilterDirective {
	fields := make([]*fieldMatch, len(d.fields))
	for i, field := range d.fields {
		copied := *field
		fields[i] = &copied
	}
	return &FilterDirective{inSpan: d.inSpan, fields: fields, target: d.target, level: d.level}
}

// ===== impl Dynamics =====

// dynamicsMatcher is Dynamics::matcher: what the dynamic directives that
// care about a span callsite need of it.
func dynamicsMatcher(s *directiveSet[*FilterDirective], metadata *tracing.Metadata) rs.Option[*callsiteMatcher] {
	baseLevel := rs.None[tracing.LevelFilter]()
	var fieldMatches []*callsiteMatch
	for _, d := range s.directivesFor(metadata) {
		if f, ok := d.fieldMatcher(metadata).Get(); ok {
			fieldMatches = append(fieldMatches, f)
			continue
		}
		if b, ok := baseLevel.Get(); ok {
			if d.level > b {
				baseLevel = rs.Some(d.level)
			}
		} else {
			baseLevel = rs.Some(d.level)
		}
	}

	if level, ok := baseLevel.Get(); ok {
		return rs.Some(&callsiteMatcher{fieldMatches: fieldMatches, baseLevel: level})
	} else if len(fieldMatches) != 0 {
		return rs.Some(&callsiteMatcher{fieldMatches: fieldMatches, baseLevel: tracing.LevelFilterOff})
	}
	return rs.None[*callsiteMatcher]()
}

// dynamicsHasValueFilters is Dynamics::has_value_filters.
func dynamicsHasValueFilters(s *directiveSet[*FilterDirective]) bool {
	for _, d := range s.directives {
		for _, f := range d.fields {
			if f.value.IsSome() {
				return true
			}
		}
	}
	return false
}

// ===== impl CallsiteMatcher =====

// toSpanMatch is CallsiteMatcher::to_span_match: the matches for one span,
// with the values it was made with already recorded.
func callsiteMatcherToSpanMatch(m *callsiteMatcher, attrs *tracing.SpanAttributes) *spanMatcher {
	fieldMatches := make([]*spanMatch, 0, len(m.fieldMatches))
	for _, cm := range m.fieldMatches {
		sm := cm.toSpanMatch()
		attrs.Record(sm.visitor())
		fieldMatches = append(fieldMatches, sm)
	}
	return &spanMatcher{fieldMatches: fieldMatches, baseLevel: m.baseLevel}
}

// ===== impl SpanMatcher =====

// spanMatcherLevel is SpanMatcher::level: the most verbose level of the
// matches the span satisfies, or the base level.
func spanMatcherLevel(m *spanMatcher) tracing.LevelFilter {
	level := rs.None[tracing.LevelFilter]()
	for _, sm := range m.fieldMatches {
		if filter, ok := sm.filter().Get(); ok {
			if current, ok := level.Get(); !ok || filter >= current {
				level = rs.Some(filter)
			}
		}
	}
	return level.UnwrapOr(m.baseLevel)
}

// spanMatcherRecordUpdate is SpanMatcher::record_update.
func spanMatcherRecordUpdate(m *spanMatcher, record *tracing.SpanRecord) {
	for _, sm := range m.fieldMatches {
		record.Record(sm.visitor())
	}
}
