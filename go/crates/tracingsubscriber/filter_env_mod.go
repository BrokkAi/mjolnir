// Port of tracing-subscriber 0.3.23 src/filter/env/mod.rs

package tracingsubscriber

import (
	"reflect"
	"slices"
	"strings"
	"sync"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// EnvFilter is tracing_subscriber::EnvFilter: a filter of spans and events
// by directives of the form `target[span{field=value}]=level`, as RUST_LOG
// holds them.
//
// A static directive (a target and a level, or field names without values)
// decides from the callsite alone. A dynamic one (a span name, or field
// values) enables what happens inside a matching span, so the filter keeps
// the spans that match and, per thread, the levels of those entered.
type EnvFilter struct {
	statics     directiveSet[*staticDirective]
	dynamics    directiveSet[*FilterDirective]
	hasDynamics bool
	byIdLock    sync.RWMutex
	byId        map[tracing.SpanId]*spanMatcher
	byCsLock    sync.RWMutex
	byCs        map[tracing.CallsiteIdentifier]*callsiteMatcher
	// scope (a ThreadLocal<RefCell<Vec<LevelFilter>>>) is kept in thread-local
	// storage under envFilterScopeKey.
	regex bool
}

// envFilterScopeKey is the key of an EnvFilter's scope in thread-local
// storage.
type envFilterScopeKey struct {
	filter *EnvFilter
}

// FilterFromEnvError is tracing_subscriber::filter::FromEnvError: the error
// of making a filter from an environment variable.
type FilterFromEnvError struct {
	// parse is ErrorKind::Parse; nil means ErrorKind::Env.
	parse *FilterParseError
	env   *envVarError
}

// Clone is `impl Clone for EnvFilter`: the directives, with none of the
// spans the original has seen.
func (f *EnvFilter) Clone() *EnvFilter {
	return &EnvFilter{
		statics:     directiveSet[*staticDirective]{directives: slices.Clone(f.statics.directives), maxLevel: f.statics.maxLevel},
		dynamics:    directiveSet[*FilterDirective]{directives: slices.Clone(f.dynamics.directives), maxLevel: f.dynamics.maxLevel},
		hasDynamics: f.hasDynamics,
		byId:        make(map[tracing.SpanId]*spanMatcher),
		byCs:        make(map[tracing.CallsiteIdentifier]*callsiteMatcher),
		regex:       f.regex,
	}
}

// EnvFilterDEFAULT_ENV is EnvFilter::DEFAULT_ENV.
const EnvFilterDEFAULT_ENV = "RUST_LOG"

// EnvFilterBuilder is EnvFilter::builder.
func EnvFilterBuilder() FilterBuilder {
	return FilterBuilderDefault()
}

// EnvFilterFromDefaultEnv is EnvFilter::from_default_env: the directives in
// RUST_LOG, error when there are none; invalid ones are reported on
// standard error and left out.
func EnvFilterFromDefaultEnv() *EnvFilter {
	return EnvFilterBuilder().
		WithDefaultDirective(FilterDirectiveFromLevelFilter(tracing.LevelFilterError)).
		FromEnvLossy()
}

// EnvFilterFromEnv is EnvFilter::from_env: EnvFilterFromDefaultEnv with
// another variable.
func EnvFilterFromEnv(env string) *EnvFilter {
	return EnvFilterBuilder().
		WithDefaultDirective(FilterDirectiveFromLevelFilter(tracing.LevelFilterError)).
		WithEnvVar(env).
		FromEnvLossy()
}

// EnvFilterNew is EnvFilter::new, and `EnvFilter::from(text)`: the
// directives in the text, error when there are none; invalid ones are
// reported on standard error and left out.
func EnvFilterNew(directives string) *EnvFilter {
	return EnvFilterBuilder().
		WithDefaultDirective(FilterDirectiveFromLevelFilter(tracing.LevelFilterError)).
		ParseLossy(directives)
}

// EnvFilterTryNew is EnvFilter::try_new: the directives in the text, or the
// error of the first that does not parse. An empty text enables nothing.
func EnvFilterTryNew(dirs string) (*EnvFilter, error) {
	return EnvFilterBuilder().Parse(dirs)
}

// EnvFilterTryFromDefaultEnv is EnvFilter::try_from_default_env.
func EnvFilterTryFromDefaultEnv() (*EnvFilter, error) {
	return EnvFilterBuilder().TryFromEnv()
}

// EnvFilterTryFromEnv is EnvFilter::try_from_env.
func EnvFilterTryFromEnv(env string) (*EnvFilter, error) {
	return EnvFilterBuilder().WithEnvVar(env).TryFromEnv()
}

// AddDirective is EnvFilter::add_directive: the filter with one more
// directive.
func (f *EnvFilter) AddDirective(directive *FilterDirective) *EnvFilter {
	if !f.regex {
		directive.deregexify()
	}
	if stat, ok := directive.toStatic().Get(); ok {
		f.statics.add(stat)
	} else {
		f.hasDynamics = true
		f.dynamics.add(directive)
	}
	return f
}

// enabled is EnvFilter::enabled.
func (f *EnvFilter) enabled(metadata *tracing.Metadata) bool {
	level := metadata.Level()

	// is it possible for a dynamic filter directive to enable this event?
	// if not, we can avoid the thread loca'l access + iterating over the
	// spans in the current scope.
	if f.hasDynamics && f.dynamics.maxLevel.Enables(level) {
		if metadata.IsSpan() {
			// If the metadata is a span, see if we care about its callsite.
			f.byCsLock.RLock()
			_, enabledByCs := f.byCs[metadata.Callsite()]
			f.byCsLock.RUnlock()
			if enabledByCs {
				return true
			}
		}

		if scope := f.scope(tracing.ThreadLocals(), false); scope != nil {
			for _, filter := range *scope {
				if filter.Enables(level) {
					return true
				}
			}
		}
	}

	// is it possible for a static filter directive to enable this event?
	if f.statics.maxLevel.Enables(level) {
		// Otherwise, fall back to checking if the callsite is
		// statically enabled.
		return staticsEnabled(&f.statics, metadata)
	}

	return false
}

// MaxLevelHint is EnvFilter::max_level_hint: trace when a directive tests a
// field's value, else the most verbose level of any directive.
func (f *EnvFilter) MaxLevelHint() rs.Option[tracing.LevelFilter] {
	if dynamicsHasValueFilters(&f.dynamics) {
		// If we perform any filtering on span field *values*, we will
		// enable *all* spans, because their field values are not known
		// until recording.
		return rs.Some(tracing.LevelFilterTrace)
	}
	return rs.Some(max(f.statics.maxLevel, f.dynamics.maxLevel))
}

func (f *EnvFilter) onNewSpan(attrs *tracing.SpanAttributes, id tracing.SpanId) {
	f.byCsLock.RLock()
	cs, ok := f.byCs[attrs.Metadata().Callsite()]
	f.byCsLock.RUnlock()
	if ok {
		span := callsiteMatcherToSpanMatch(cs, attrs)
		f.byIdLock.Lock()
		f.byId[id] = span
		f.byIdLock.Unlock()
	}
}

func (f *EnvFilter) onEnter(id tracing.SpanId) {
	// XXX: This is where _we_ could push IDs to the stack instead, and use
	// that to allow changing the filter while a span is already entered.
	// But that might be much less efficient...
	f.byIdLock.RLock()
	span, ok := f.byId[id]
	f.byIdLock.RUnlock()
	if ok {
		locals := tracing.ThreadLocals()
		scope := f.scope(locals, true)
		*scope = append(*scope, spanMatcherLevel(span))
	}
}

func (f *EnvFilter) onExit(id tracing.SpanId) {
	if f.caresAboutSpan(id) {
		locals := tracing.ThreadLocals()
		if scope := f.scope(locals, true); len(*scope) > 0 {
			*scope = (*scope)[:len(*scope)-1]
			if len(*scope) == 0 {
				locals.Delete(envFilterScopeKey{filter: f})
			}
		}
	}
}

func (f *EnvFilter) onClose(id tracing.SpanId) {
	// If we don't need to acquire a write lock, avoid doing so.
	if !f.caresAboutSpan(id) {
		return
	}
	f.byIdLock.Lock()
	delete(f.byId, id)
	f.byIdLock.Unlock()
}

func (f *EnvFilter) onRecord(id tracing.SpanId, values *tracing.SpanRecord) {
	f.byIdLock.RLock()
	span, ok := f.byId[id]
	f.byIdLock.RUnlock()
	if ok {
		spanMatcherRecordUpdate(span, values)
	}
}

func (f *EnvFilter) caresAboutSpan(span tracing.SpanId) bool {
	f.byIdLock.RLock()
	defer f.byIdLock.RUnlock()
	_, ok := f.byId[span]
	return ok
}

func (f *EnvFilter) baseInterest() tracing.SubscriberInterest {
	if f.hasDynamics {
		return tracing.SubscriberInterestSometimes()
	}
	return tracing.SubscriberInterestNever()
}

// scope is the calling thread's levels of entered spans this filter matched.
func (f *EnvFilter) scope(locals tracing.LocalStore, create bool) *[]tracing.LevelFilter {
	if stored, ok := locals.Get(envFilterScopeKey{filter: f}); ok {
		return stored.(*[]tracing.LevelFilter)
	}
	if !create {
		return nil
	}
	scope := new([]tracing.LevelFilter)
	locals.Set(envFilterScopeKey{filter: f}, scope)
	return scope
}

// RegisterCallsite is EnvFilter::register_callsite, `impl Layer for
// EnvFilter`.
func (f *EnvFilter) RegisterCallsite(metadata *tracing.Metadata) tracing.SubscriberInterest {
	if f.hasDynamics && metadata.IsSpan() {
		// If this metadata describes a span, first, check if there is a
		// dynamic filter that should be constructed for it. If so, it
		// should always be enabled, since it influences filtering.
		if matcher, ok := dynamicsMatcher(&f.dynamics, metadata).Get(); ok {
			f.byCsLock.Lock()
			f.byCs[metadata.Callsite()] = matcher
			f.byCsLock.Unlock()
			return tracing.SubscriberInterestAlways()
		}
	}

	// Otherwise, check if any of our static filters enable this metadata.
	if staticsEnabled(&f.statics, metadata) {
		return tracing.SubscriberInterestAlways()
	}
	return f.baseInterest()
}

// ===== impl Layer for EnvFilter =====

func (f *EnvFilter) OnRegisterDispatch(subscriber *tracing.Dispatch) {
	LayerDefaultOnRegisterDispatch(f, subscriber)
}

func (f *EnvFilter) OnLayer(subscriber tracing.Subscriber) { LayerDefaultOnLayer(f, subscriber) }

// Enabled is `impl Layer for EnvFilter`.
func (f *EnvFilter) Enabled(metadata *tracing.Metadata, ctx LayerContext) bool {
	return f.enabled(metadata)
}

// OnNewSpan is `impl Layer for EnvFilter`.
func (f *EnvFilter) OnNewSpan(attrs *tracing.SpanAttributes, id tracing.SpanId, ctx LayerContext) {
	f.onNewSpan(attrs, id)
}

// OnRecord is `impl Layer for EnvFilter`.
func (f *EnvFilter) OnRecord(span tracing.SpanId, values *tracing.SpanRecord, ctx LayerContext) {
	f.onRecord(span, values)
}

func (f *EnvFilter) OnFollowsFrom(span tracing.SpanId, follows tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnFollowsFrom(f, span, follows, ctx)
}

func (f *EnvFilter) EventEnabled(event *tracing.Event, ctx LayerContext) bool {
	return LayerDefaultEventEnabled(f, event, ctx)
}

func (f *EnvFilter) OnEvent(event *tracing.Event, ctx LayerContext) {
	LayerDefaultOnEvent(f, event, ctx)
}

// OnEnter is `impl Layer for EnvFilter`.
func (f *EnvFilter) OnEnter(id tracing.SpanId, ctx LayerContext) { f.onEnter(id) }

// OnExit is `impl Layer for EnvFilter`.
func (f *EnvFilter) OnExit(id tracing.SpanId, ctx LayerContext) { f.onExit(id) }

// OnClose is `impl Layer for EnvFilter`.
func (f *EnvFilter) OnClose(id tracing.SpanId, ctx LayerContext) { f.onClose(id) }

func (f *EnvFilter) OnIdChange(old tracing.SpanId, new tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnIdChange(f, old, new, ctx)
}

func (f *EnvFilter) DowncastRaw(target reflect.Type) (any, bool) {
	return LayerDefaultDowncastRaw(f, target)
}

// EnvFilterFromStr is `impl FromStr for EnvFilter`: EnvFilterTryNew.
func EnvFilterFromStr(spec string) (*EnvFilter, error) {
	return EnvFilterTryNew(spec)
}

// EnvFilterDefault is `impl Default for EnvFilter`: no directives, so
// nothing is enabled.
func EnvFilterDefault() *EnvFilter {
	return FilterBuilderDefault().fromDirectives(nil)
}

// String is `impl Display for EnvFilter`: the static directives, then the
// dynamic ones, most specific first, separated by commas.
func (f *EnvFilter) String() string {
	var parts []string
	for _, directive := range f.statics.iter() {
		parts = append(parts, directive.String())
	}
	for _, directive := range f.dynamics.iter() {
		parts = append(parts, directive.String())
	}
	return strings.Join(parts, ",")
}

// FmtDisplay is `impl Display for EnvFilter`: the formatter's flags apply to
// the first static and the first dynamic directive.
func (f *EnvFilter) FmtDisplay(formatter *rs.Formatter) error {
	statics := f.statics.iter()
	wroteStatics := false
	if len(statics) > 0 {
		if err := statics[0].fmtDisplay(formatter); err != nil {
			return err
		}
		for _, directive := range statics[1:] {
			if err := formatter.WriteStr("," + directive.String()); err != nil {
				return err
			}
		}
		wroteStatics = true
	}

	dynamics := f.dynamics.iter()
	if len(dynamics) > 0 {
		if wroteStatics {
			if err := formatter.WriteStr(","); err != nil {
				return err
			}
		}
		if err := dynamics[0].FmtDisplay(formatter); err != nil {
			return err
		}
		for _, directive := range dynamics[1:] {
			if err := formatter.WriteStr("," + directive.String()); err != nil {
				return err
			}
		}
	}
	return nil
}

// ===== impl FromEnvError =====

func filterFromEnvErrorFromParse(p *FilterParseError) *FilterFromEnvError {
	return &FilterFromEnvError{parse: p}
}

func filterFromEnvErrorFromVar(v *envVarError) *FilterFromEnvError {
	return &FilterFromEnvError{env: v}
}

func (e *FilterFromEnvError) Error() string { return e.String() }

// String is `impl Display for FromEnvError`.
func (e *FilterFromEnvError) String() string {
	if e.parse != nil {
		return e.parse.String()
	}
	return e.env.Error()
}

// FmtDisplay is `impl Display for FromEnvError`.
func (e *FilterFromEnvError) FmtDisplay(f *rs.Formatter) error {
	if e.parse != nil {
		return e.parse.FmtDisplay(f)
	}
	return f.WriteStr(e.env.Error())
}

// Unwrap is FromEnvError::source.
func (e *FilterFromEnvError) Unwrap() error {
	if e.parse != nil {
		return e.parse
	}
	return e.env
}
