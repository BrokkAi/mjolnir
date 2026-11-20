// Port of tracing-subscriber 0.3.23 src/fmt/mod.rs

package tracingsubscriber

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// FmtSubscriber is tracing_subscriber::fmt::Subscriber: a filter layered
// over the fmt layer layered over a Registry.
type FmtSubscriber struct {
	inner *LayerLayered
}

// FmtSubscriberBuilder is tracing_subscriber::fmt::SubscriberBuilder: how a
// FmtSubscriber is configured. Each method returns the changed builder, as
// the Rust methods return a new one.
//
//	tracingsubscriber.Fmt().
//		WithAnsi(false).
//		WithEnvFilter(filter).
//		WithWriter(writer).
//		TryInit()
type FmtSubscriberBuilder struct {
	filter Layer
	inner  FmtLayer
}

// Fmt is tracing_subscriber::fmt(): a builder with the defaults: the Full
// format with the system time, events up to INFO, written to standard
// output, in colour unless NO_COLOR is set.
func Fmt() FmtSubscriberBuilder {
	return FmtSubscriberBuilderDefault()
}

// FmtSubscriberDEFAULT_MAX_LEVEL is Subscriber::DEFAULT_MAX_LEVEL.
const FmtSubscriberDEFAULT_MAX_LEVEL = tracing.LevelFilterInfo

// FmtSubscriberNew is Subscriber::new and `impl Default for Subscriber`: the
// default builder, finished.
func FmtSubscriberNew() *FmtSubscriber {
	return FmtSubscriberBuilderDefault().Finish()
}

// ===== impl Subscriber for fmt::Subscriber =====

// OnRegisterDispatch is the default Subscriber::on_register_dispatch: the
// fmt subscriber does not forward it.
func (s *FmtSubscriber) OnRegisterDispatch(subscriber *tracing.Dispatch) {
	tracing.SubscriberDefaultOnRegisterDispatch(s, subscriber)
}

func (s *FmtSubscriber) RegisterCallsite(meta *tracing.Metadata) tracing.SubscriberInterest {
	return s.inner.RegisterCallsite(meta)
}

func (s *FmtSubscriber) Enabled(meta *tracing.Metadata) bool {
	return s.inner.Enabled(meta)
}

func (s *FmtSubscriber) NewSpan(attrs *tracing.SpanAttributes) tracing.SpanId {
	return s.inner.NewSpan(attrs)
}

func (s *FmtSubscriber) Record(span tracing.SpanId, values *tracing.SpanRecord) {
	s.inner.Record(span, values)
}

func (s *FmtSubscriber) RecordFollowsFrom(span tracing.SpanId, follows tracing.SpanId) {
	s.inner.RecordFollowsFrom(span, follows)
}

func (s *FmtSubscriber) EventEnabled(event *tracing.Event) bool {
	return s.inner.EventEnabled(event)
}

func (s *FmtSubscriber) Event(event *tracing.Event) {
	s.inner.Event(event)
}

func (s *FmtSubscriber) Enter(id tracing.SpanId) {
	s.inner.Enter(id)
}

func (s *FmtSubscriber) Exit(id tracing.SpanId) {
	s.inner.Exit(id)
}

func (s *FmtSubscriber) CurrentSpan() tracing.CurrentSpan {
	return s.inner.CurrentSpan()
}

func (s *FmtSubscriber) CloneSpan(id tracing.SpanId) tracing.SpanId {
	return s.inner.CloneSpan(id)
}

// DropSpan is the default Subscriber::drop_span: nothing.
func (s *FmtSubscriber) DropSpan(id tracing.SpanId) {
	tracing.SubscriberDefaultDropSpan(s, id)
}

func (s *FmtSubscriber) TryClose(id tracing.SpanId) bool {
	return s.inner.TryClose(id)
}

func (s *FmtSubscriber) MaxLevelHint() rs.Option[tracing.LevelFilter] {
	return s.inner.MaxLevelHint()
}

// DowncastRaw is `impl Subscriber for fmt::Subscriber`: the subscriber, or
// any of its parts.
func (s *FmtSubscriber) DowncastRaw(target reflect.Type) (any, bool) {
	if target == reflect.TypeFor[*FmtSubscriber]() {
		return s, true
	}
	return s.inner.DowncastRaw(target)
}

// SpanData is `impl LookupSpan for fmt::Subscriber`.
func (s *FmtSubscriber) SpanData(id tracing.SpanId) rs.Option[*RegistryData] {
	return s.inner.SpanData(id)
}

// TryInit is SubscriberInitExt::try_init: the subscriber becomes the global
// default.
func (s *FmtSubscriber) TryInit() error {
	return SubscriberInitExtTryInit(s)
}

// Init is SubscriberInitExt::init.
func (s *FmtSubscriber) Init() {
	SubscriberInitExtInit(s)
}

// SetDefault is SubscriberInitExt::set_default: the subscriber becomes the
// default of the calling thread until the guard is dropped.
func (s *FmtSubscriber) SetDefault() *tracing.DispatcherDefaultGuard {
	return SubscriberInitExtSetDefault(s)
}

// ===== impl SubscriberBuilder =====

// FmtSubscriberBuilderDefault is `impl Default for SubscriberBuilder`: it
// logs its own write errors on standard error.
func FmtSubscriberBuilderDefault() FmtSubscriberBuilder {
	return FmtSubscriberBuilder{
		filter: FilterLevelFilter{Filter: FmtSubscriberDEFAULT_MAX_LEVEL},
		inner:  FmtLayerDefault(),
	}.LogInternalErrors(true)
}

// Finish is SubscriberBuilder::finish: the subscriber.
func (b FmtSubscriberBuilder) Finish() *FmtSubscriber {
	layer := b.inner
	subscriber := LayerWithSubscriber(&layer, RegistryDefault())
	return &FmtSubscriber{inner: LayerWithSubscriber(b.filter, subscriber)}
}

// TryInit is SubscriberBuilder::try_init: the subscriber becomes the global
// default. It fails when one was already set.
func (b FmtSubscriberBuilder) TryInit() error {
	if err := b.Finish().TryInit(); err != nil {
		return err
	}
	return nil
}

// Init is SubscriberBuilder::init: TryInit, panicking on failure.
func (b FmtSubscriberBuilder) Init() {
	if err := b.TryInit(); err != nil {
		rs.PanicAt(1, "Unable to install global subscriber: "+rs.DebugString(err))
	}
}

// WithTimer is SubscriberBuilder::with_timer.
func (b FmtSubscriberBuilder) WithTimer(timer FmtTimeFormatTime) FmtSubscriberBuilder {
	b.inner = b.inner.WithTimer(timer)
	return b
}

// WithoutTime is SubscriberBuilder::without_time.
func (b FmtSubscriberBuilder) WithoutTime() FmtSubscriberBuilder {
	b.inner = b.inner.WithoutTime()
	return b
}

// WithSpanEvents is SubscriberBuilder::with_span_events.
func (b FmtSubscriberBuilder) WithSpanEvents(kind FmtFormatFmtSpan) FmtSubscriberBuilder {
	b.inner = b.inner.WithSpanEvents(kind)
	return b
}

// WithAnsi is SubscriberBuilder::with_ansi.
func (b FmtSubscriberBuilder) WithAnsi(ansi bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithAnsi(ansi)
	return b
}

// WithAnsiSanitization is SubscriberBuilder::with_ansi_sanitization.
func (b FmtSubscriberBuilder) WithAnsiSanitization(ansiSanitization bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithAnsiSanitization(ansiSanitization)
	return b
}

// LogInternalErrors is SubscriberBuilder::log_internal_errors.
func (b FmtSubscriberBuilder) LogInternalErrors(logInternalErrors bool) FmtSubscriberBuilder {
	b.inner = b.inner.LogInternalErrors(logInternalErrors)
	return b
}

// WithTarget is SubscriberBuilder::with_target.
func (b FmtSubscriberBuilder) WithTarget(displayTarget bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithTarget(displayTarget)
	return b
}

// WithFile is SubscriberBuilder::with_file.
func (b FmtSubscriberBuilder) WithFile(displayFilename bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithFile(displayFilename)
	return b
}

// WithLineNumber is SubscriberBuilder::with_line_number.
func (b FmtSubscriberBuilder) WithLineNumber(displayLineNumber bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithLineNumber(displayLineNumber)
	return b
}

// WithLevel is SubscriberBuilder::with_level.
func (b FmtSubscriberBuilder) WithLevel(displayLevel bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithLevel(displayLevel)
	return b
}

// WithThreadNames is SubscriberBuilder::with_thread_names.
func (b FmtSubscriberBuilder) WithThreadNames(displayThreadNames bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithThreadNames(displayThreadNames)
	return b
}

// WithThreadIds is SubscriberBuilder::with_thread_ids.
func (b FmtSubscriberBuilder) WithThreadIds(displayThreadIds bool) FmtSubscriberBuilder {
	b.inner = b.inner.WithThreadIds(displayThreadIds)
	return b
}

// WithEnvFilter is SubscriberBuilder::with_env_filter: filter is an
// *EnvFilter, or a string, which becomes EnvFilterNew(filter) as
// `Into<EnvFilter>` makes one.
func (b FmtSubscriberBuilder) WithEnvFilter(filter any) FmtSubscriberBuilder {
	switch filter := filter.(type) {
	case *EnvFilter:
		b.filter = filter
	case string:
		b.filter = EnvFilterNew(filter)
	default:
		panic("tracing_subscriber: with_env_filter needs an EnvFilter or a string, not " +
			reflect.TypeOf(filter).String())
	}
	return b
}

// WithMaxLevel is SubscriberBuilder::with_max_level: filter is a
// tracing.LevelFilter or a tracing.Level.
func (b FmtSubscriberBuilder) WithMaxLevel(filter any) FmtSubscriberBuilder {
	switch filter := filter.(type) {
	case tracing.LevelFilter:
		b.filter = FilterLevelFilter{Filter: filter}
	case tracing.Level:
		b.filter = FilterLevelFilter{Filter: tracing.LevelFilterFromLevel(filter)}
	default:
		panic("tracing_subscriber: with_max_level needs a LevelFilter or a Level, not " +
			reflect.TypeOf(filter).String())
	}
	return b
}

// WithWriter is SubscriberBuilder::with_writer: makeWriter is a
// FmtMakeWriter, or a function of no arguments returning an io.Writer.
func (b FmtSubscriberBuilder) WithWriter(makeWriter any) FmtSubscriberBuilder {
	b.inner = b.inner.WithWriter(makeWriter)
	return b
}
