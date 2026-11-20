// Port of tracing-subscriber 0.3.23 src/filter/directive.rs

package tracingsubscriber

import (
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// FilterParseError is tracing_subscriber::filter::ParseError: a filter
// directive that does not parse.
type FilterParseError struct {
	kind parseErrorKind
}

// staticDirective is StaticDirective: a directive that depends only on a
// callsite's target, field names and level.
type staticDirective struct {
	target     rs.Option[string]
	fieldNames []string
	level      tracing.LevelFilter
}

// directiveSet is DirectiveSet: directives kept in order, most specific
// first, with the most verbose level any of them enables.
type directiveSet[T filterMatch[T]] struct {
	directives []T
	maxLevel   tracing.LevelFilter
}

// filterMatch is the Match trait, with the Ord the set is sorted by.
type filterMatch[T any] interface {
	caresAbout(meta *tracing.Metadata) bool
	matchLevel() tracing.LevelFilter
	compare(other T) int
}

type parseErrorKind struct {
	// field is ParseErrorKind::Field: the error of a field filter.
	field error
	// level is ParseErrorKind::Level.
	level *tracing.ParseLevelFilterError
	// other is ParseErrorKind::Other, with its optional message.
	other rs.Option[string]
}

func (s *directiveSet[T]) isEmpty() bool {
	return len(s.directives) == 0
}

func (s *directiveSet[T]) iter() []T {
	return s.directives
}

// directivesFor is DirectiveSet::directives_for: the directives that care
// about a callsite, most specific first.
func (s *directiveSet[T]) directivesFor(metadata *tracing.Metadata) []T {
	var matching []T
	for _, d := range s.directives {
		if d.caresAbout(metadata) {
			matching = append(matching, d)
		}
	}
	return matching
}

// add is DirectiveSet::add: a directive equal in order to one already in the
// set replaces it. The set's maximum level counts the replaced one too.
func (s *directiveSet[T]) add(directive T) {
	level := directive.matchLevel()
	if level > s.maxLevel {
		s.maxLevel = level
	}
	i, found := slices.BinarySearchFunc(s.directives, directive, func(existing, target T) int {
		return existing.compare(target)
	})
	if found {
		s.directives[i] = directive
	} else {
		s.directives = slices.Insert(s.directives, i, directive)
	}
}

// directiveSetFrom is `impl FromIterator for DirectiveSet`.
func directiveSetFrom[T filterMatch[T]](directives []T) directiveSet[T] {
	this := directiveSet[T]{maxLevel: tracing.LevelFilterOff}
	for _, directive := range directives {
		this.add(directive)
	}
	return this
}

// enabled is DirectiveSet<StaticDirective>::enabled: the most specific
// directive that cares about the callsite decides.
func staticsEnabled(s *directiveSet[*staticDirective], meta *tracing.Metadata) bool {
	level := meta.Level()
	for _, d := range s.directives {
		if d.caresAbout(meta) {
			return d.level.Enables(level)
		}
	}
	return false
}

// ===== impl StaticDirective =====

func staticDirectiveNew(target rs.Option[string], fieldNames []string, level tracing.LevelFilter) *staticDirective {
	return &staticDirective{target: target, fieldNames: fieldNames, level: level}
}

func (d *staticDirective) compare(other *staticDirective) int {
	ordering := cmpOptionLen(d.target, other.target)
	if ordering == 0 {
		ordering = cmpInt(len(d.fieldNames), len(other.fieldNames))
	}
	if ordering == 0 {
		ordering = cmpOptionString(d.target, other.target)
		if ordering == 0 {
			ordering = slices.Compare(d.fieldNames, other.fieldNames)
		}
	}
	ordering = -ordering
	if rs.DebugAssertions && ordering == 0 {
		rs.DebugAssert(d.target == other.target,
			"invariant violated: Ordering::Equal must imply a.target == b.target")
		rs.DebugAssert(slices.Equal(d.fieldNames, other.fieldNames),
			"invariant violated: Ordering::Equal must imply a.field_names == b.field_names")
	}
	return ordering
}

func (d *staticDirective) caresAbout(meta *tracing.Metadata) bool {
	// Does this directive have a target filter, and does it match the
	// metadata's target?
	if target, ok := d.target.Get(); ok {
		if !strings.HasPrefix(meta.Target(), target) {
			return false
		}
	}

	if meta.IsEvent() && len(d.fieldNames) != 0 {
		fields := meta.Fields()
		for _, name := range d.fieldNames {
			if fields.Field(name).IsNone() {
				return false
			}
		}
	}

	return true
}

func (d *staticDirective) matchLevel() tracing.LevelFilter { return d.level }

// String is `impl Display for StaticDirective`.
func (d *staticDirective) String() string {
	var f strings.Builder
	wroteAny := false
	if target, ok := d.target.Get(); ok {
		f.WriteString(target)
		wroteAny = true
	}

	if len(d.fieldNames) != 0 {
		f.WriteString("[")
		f.WriteString("{" + strings.Join(d.fieldNames, ",") + "}")
		f.WriteString("]")
		wroteAny = true
	}

	if wroteAny {
		f.WriteString("=")
	}

	f.WriteString(d.level.String())
	return f.String()
}

// fmtDisplay is `impl Display for StaticDirective` with the formatter's
// flags, which apply to the target and the level.
func (d *staticDirective) fmtDisplay(f *rs.Formatter) error {
	wroteAny := false
	if target, ok := d.target.Get(); ok {
		if err := rs.DisplayFmt(target, f); err != nil {
			return err
		}
		wroteAny = true
	}

	if len(d.fieldNames) != 0 {
		if err := f.WriteStr("[{" + strings.Join(d.fieldNames, ",") + "}]"); err != nil {
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

// ===== impl ParseError =====

// filterParseErrorNew is ParseError::new: "invalid filter directive".
func filterParseErrorNew() *FilterParseError {
	return &FilterParseError{kind: parseErrorKind{other: rs.None[string]()}}
}

// filterParseErrorMsg is ParseError::msg.
func filterParseErrorMsg(s string) *FilterParseError {
	return &FilterParseError{kind: parseErrorKind{other: rs.Some(s)}}
}

// filterParseErrorFromField is `From<Box<dyn Error>> for ParseError`.
func filterParseErrorFromField(err error) *FilterParseError {
	return &FilterParseError{kind: parseErrorKind{field: err}}
}

// filterParseErrorFromLevel is `From<level::ParseError> for ParseError`.
func filterParseErrorFromLevel(err *tracing.ParseLevelFilterError) *FilterParseError {
	return &FilterParseError{kind: parseErrorKind{level: err}}
}

func (e *FilterParseError) Error() string { return e.String() }

// String is `impl Display for ParseError`.
func (e *FilterParseError) String() string {
	switch {
	case e.kind.field != nil:
		return "invalid field filter: " + e.kind.field.Error()
	case e.kind.level != nil:
		return e.kind.level.String()
	}
	if msg, ok := e.kind.other.Get(); ok {
		return "invalid filter directive: " + msg
	}
	return "invalid filter directive"
}

// FmtDisplay is `impl Display for ParseError`: the message without a reason
// pads, as `f.pad` does; the others are written as they are.
func (e *FilterParseError) FmtDisplay(f *rs.Formatter) error {
	switch {
	case e.kind.field != nil:
		return f.WriteStr(e.String())
	case e.kind.level != nil:
		return e.kind.level.FmtDisplay(f)
	}
	if e.kind.other.IsSome() {
		return f.WriteStr(e.String())
	}
	return f.Pad(e.String())
}

// FmtDebug is the derived Debug of ParseError.
func (e *FilterParseError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ParseError").Field("kind", parseErrorKindDebug{e.kind}).Finish()
}

type parseErrorKindDebug struct{ kind parseErrorKind }

func (k parseErrorKindDebug) FmtDebug(f *rs.Formatter) error {
	switch {
	case k.kind.field != nil:
		return f.DebugTuple("Field").Field(k.kind.field).Finish()
	case k.kind.level != nil:
		return f.DebugTuple("Level").Field(k.kind.level).Finish()
	}
	return f.DebugTuple("Other").Field(k.kind.other).Finish()
}

// Unwrap is ParseError::source.
func (e *FilterParseError) Unwrap() error {
	switch {
	case e.kind.field != nil:
		return e.kind.field
	case e.kind.level != nil:
		return e.kind.level
	}
	return nil
}

// ===== helpers for the orderings =====

func cmpInt(a, b int) int {
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	}
	return 0
}

// cmpOptionLen is `a.as_ref().map(String::len).cmp(&b.as_ref().map(String::len))`.
func cmpOptionLen(a, b rs.Option[string]) int {
	x, aOk := a.Get()
	y, bOk := b.Get()
	switch {
	case !aOk && !bOk:
		return 0
	case !aOk:
		return -1
	case !bOk:
		return 1
	}
	return cmpInt(len(x), len(y))
}

// cmpOptionString is `a.cmp(&b)` for two Option<String>.
func cmpOptionString(a, b rs.Option[string]) int {
	x, aOk := a.Get()
	y, bOk := b.Get()
	switch {
	case !aOk && !bOk:
		return 0
	case !aOk:
		return -1
	case !bOk:
		return 1
	}
	return strings.Compare(x, y)
}
