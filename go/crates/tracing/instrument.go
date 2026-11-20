// Port of tracing 0.1.44 src/instrument.rs

package tracing

import "context"

// A future of the port is a `func(ctx context.Context) T` (tokio.Fn[T] is
// that type), so these take and return one. They do not import the async
// runtime: the runtime tells this package, through RuntimeHooks.PollScope,
// how to act at each poll. Without a runtime the future is a plain call, so
// the span is entered (or the dispatcher set) once around it.

// Instrument is `future.instrument(span)` (the Instrument trait): the future,
// run with span entered while it is polled. The result owns the span and
// drops it when the future completes, as dropping Instrumented does.
//
//	tasks.Spawn(tracing.Instrument(refreshProfile(request), span))
func Instrument[T any](future func(ctx context.Context) T, span *Span) func(ctx context.Context) T {
	return func(ctx context.Context) T {
		defer span.Drop()
		var result T
		var entered *SpanEntered
		pollScope(ctx, func() {
			entered = span.Enter()
		}, func() {
			entered.Drop()
		}, func() {
			result = future(ctx)
		})
		return result
	}
}

// InCurrentSpan is `future.in_current_span()`: Instrument with the span that
// is current now, when the future is made.
func InCurrentSpan[T any](future func(ctx context.Context) T) func(ctx context.Context) T {
	return Instrument(future, SpanCurrent())
}

// WithSubscriber is `future.with_subscriber(subscriber)` (the WithSubscriber
// trait): the future, run with the dispatcher as the default of the thread
// that polls it. subscriber is a *Dispatch, or a Subscriber, which becomes a
// new Dispatch as `Into<Dispatch>` makes one.
func WithSubscriber[T any](future func(ctx context.Context) T, subscriber any) func(ctx context.Context) T {
	return withDispatch(future, intoDispatch(subscriber))
}

// WithCurrentSubscriber is `future.with_current_subscriber()`: WithSubscriber
// with the dispatcher that is the default now, when the future is made.
func WithCurrentSubscriber[T any](future func(ctx context.Context) T) func(ctx context.Context) T {
	return withDispatch(future, DispatcherGetDefault((*Dispatch).Clone))
}

// withDispatch is `impl Future for WithDispatch`.
func withDispatch[T any](future func(ctx context.Context) T, dispatcher *Dispatch) func(ctx context.Context) T {
	return func(ctx context.Context) T {
		var result T
		var guard *DispatcherDefaultGuard
		pollScope(ctx, func() {
			guard = DispatcherSetDefault(dispatcher)
		}, func() {
			guard.Drop()
		}, func() {
			result = future(ctx)
		})
		return result
	}
}

// intoDispatch is `S: Into<Dispatch>`: a Dispatch is itself, a Subscriber
// becomes a new Dispatch.
func intoDispatch(subscriber any) *Dispatch {
	switch subscriber := subscriber.(type) {
	case *Dispatch:
		return subscriber
	case Subscriber:
		return DispatchNew(subscriber)
	}
	panic("tracing: " + "a subscriber must be a *tracing.Dispatch or implement tracing.Subscriber")
}
