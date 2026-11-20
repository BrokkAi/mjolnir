// Port of tracing-core 0.1.36 src/dispatcher.rs

package tracing

import (
	"reflect"
	"strconv"
	"sync/atomic"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Dispatch is tracing::Dispatch: a handle on a Subscriber, which is what
// events and spans are sent through. It is used by pointer and is never nil;
// Clone returns the same handle, the subscriber being shared as Rust's Arc
// shares it.
type Dispatch struct {
	subscriber Subscriber
}

// dispatcherState is the crate's thread-local State: the default dispatcher
// of the current thread, and whether a call into it is already in progress.
type dispatcherState struct {
	// default_ is None when the thread uses the global default.
	default_ *Dispatch
	// canEnter is false while this thread is inside a call to its default
	// dispatcher, so that a subscriber which itself logs does not recurse.
	canEnter bool
	// guards counts the DispatcherDefaultGuards that will restore a prior
	// default on this thread; the state is kept while there is one.
	guards int
}

var (
	// globalDispatch is GLOBAL_DISPATCH, nil until set_global_default.
	globalDispatch atomic.Pointer[Dispatch]
	// scopedCount is SCOPED_COUNT: how many scoped defaults are set, on any
	// thread. While it is zero the global default is used without looking at
	// thread state.
	scopedCount atomic.Int64

	noSubscriber SubscriberNoSubscriber
	none         = &Dispatch{subscriber: noSubscriber}
)

// dispatcherStateKey is the key of the dispatcher state in thread-local
// storage.
type dispatcherStateKey struct{}

// DispatcherDefaultGuard is tracing::dispatcher::DefaultGuard: dropping it
// puts back the default dispatcher that was current before SetDefault.
type DispatcherDefaultGuard struct {
	prior   *Dispatch
	dropped bool
}

// DispatcherWithDefault is tracing::dispatcher::with_default: it runs f with
// dispatcher as the default of the calling thread, and puts the prior default
// back when f returns or panics.
func DispatcherWithDefault[T any](dispatcher *Dispatch, f func() T) T {
	// When this guard is dropped, the default dispatcher will be reset to the
	// prior default.
	guard := DispatcherSetDefault(dispatcher)
	defer guard.Drop()
	return f()
}

// DispatcherSetDefault is tracing::dispatcher::set_default: it makes
// dispatcher the default of the calling thread until the guard is dropped.
//
// "Thread" is what ThreadLocals says it is: by default the calling
// goroutine, under the async runtime the thread a task runs on, so that
// every task of a current-thread runtime sees the default its test
// installed.
func DispatcherSetDefault(dispatcher *Dispatch) *DispatcherDefaultGuard {
	state := loadDispatcherState(ThreadLocals(), true)
	state.canEnter = true
	prior := state.default_
	state.default_ = dispatcher
	state.guards++
	scopedCount.Add(1)
	return &DispatcherDefaultGuard{prior: prior}
}

// DispatcherSetGlobalDefault is tracing::dispatcher::set_global_default: it
// makes dispatcher the default of every thread that has none of its own, for
// the rest of the program. It can be done once.
func DispatcherSetGlobalDefault(dispatcher *Dispatch) error {
	if !globalDispatch.CompareAndSwap(nil, dispatcher) {
		return &DispatcherSetGlobalDefaultError{}
	}
	CallsiteRebuildInterestCache()
	return nil
}

// DispatcherSetGlobalDefaultError is
// tracing::dispatcher::SetGlobalDefaultError.
type DispatcherSetGlobalDefaultError struct{}

const setGlobalDefaultErrorMessage = "a global default trace dispatcher has already been set"

func (e *DispatcherSetGlobalDefaultError) String() string { return setGlobalDefaultErrorMessage }

func (e *DispatcherSetGlobalDefaultError) Error() string { return setGlobalDefaultErrorMessage }

// FmtDisplay is `impl Display for SetGlobalDefaultError`.
func (e *DispatcherSetGlobalDefaultError) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(setGlobalDefaultErrorMessage)
}

// FmtDebug is `impl Debug for SetGlobalDefaultError`.
func (e *DispatcherSetGlobalDefaultError) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("SetGlobalDefaultError").Field(setGlobalDefaultErrorMessage).Finish()
}

// DispatcherGetDefault is tracing::dispatcher::get_default: it calls f with
// the current default dispatcher. A call made from inside the default
// dispatcher (a subscriber that logs while it handles an event) gets the
// dispatcher that discards everything.
func DispatcherGetDefault[T any](f func(*Dispatch) T) T {
	if scopedCount.Load() == 0 {
		// fast path if no scoped dispatcher has been set; just use the global
		// default.
		return f(getGlobal())
	}

	locals := ThreadLocals()
	state := loadDispatcherState(locals, true)
	if !state.canEnter {
		return f(none)
	}
	state.canEnter = false
	defer func() {
		state.canEnter = true
		releaseDispatcherState(locals, state)
	}()
	current := state.default_
	if current == nil {
		current = getGlobal()
	}
	return f(current)
}

func getGlobal() *Dispatch {
	if global := globalDispatch.Load(); global != nil {
		return global
	}
	return none
}

// loadDispatcherState returns the state kept in locals, the calling
// thread's storage, making it when create says so.
func loadDispatcherState(locals LocalStore, create bool) *dispatcherState {
	if stored, ok := locals.Get(dispatcherStateKey{}); ok {
		return stored.(*dispatcherState)
	}
	if !create {
		return nil
	}
	state := &dispatcherState{canEnter: true}
	locals.Set(dispatcherStateKey{}, state)
	return state
}

// releaseDispatcherState forgets a thread's state once it says nothing: no
// default of its own and no guard that will restore one.
func releaseDispatcherState(locals LocalStore, state *dispatcherState) {
	if state.default_ == nil && state.guards == 0 && state.canEnter {
		locals.Delete(dispatcherStateKey{})
	}
}

// DispatchNone is Dispatch::none: a dispatcher that discards everything.
func DispatchNone() *Dispatch {
	return &Dispatch{subscriber: noSubscriber}
}

// DispatchNew is Dispatch::new and `From<S> for Dispatch`: a dispatcher that
// forwards to subscriber.
func DispatchNew(subscriber Subscriber) *Dispatch {
	me := &Dispatch{subscriber: subscriber}
	registerDispatch(me)
	return me
}

// DispatchDefault is `impl Default for Dispatch`: the current default
// dispatcher.
func DispatchDefault() *Dispatch {
	return DispatcherGetDefault((*Dispatch).Clone)
}

// Clone returns the handle: the subscriber is shared.
func (d *Dispatch) Clone() *Dispatch { return d }

// Subscriber returns the subscriber the dispatcher forwards to.
func (d *Dispatch) Subscriber() Subscriber { return d.subscriber }

// RegisterCallsite forwards to Subscriber.RegisterCallsite.
func (d *Dispatch) RegisterCallsite(metadata *Metadata) SubscriberInterest {
	return d.subscriber.RegisterCallsite(metadata)
}

func (d *Dispatch) maxLevelHint() rs.Option[LevelFilter] {
	return d.subscriber.MaxLevelHint()
}

// NewSpan forwards to Subscriber.NewSpan.
func (d *Dispatch) NewSpan(span *SpanAttributes) SpanId { return d.subscriber.NewSpan(span) }

// Record forwards to Subscriber.Record.
func (d *Dispatch) Record(span SpanId, values *SpanRecord) { d.subscriber.Record(span, values) }

// RecordFollowsFrom forwards to Subscriber.RecordFollowsFrom.
func (d *Dispatch) RecordFollowsFrom(span SpanId, follows SpanId) {
	d.subscriber.RecordFollowsFrom(span, follows)
}

// Enabled forwards to Subscriber.Enabled.
func (d *Dispatch) Enabled(metadata *Metadata) bool { return d.subscriber.Enabled(metadata) }

// Event records an event, if Subscriber.EventEnabled lets it through.
func (d *Dispatch) Event(event *Event) {
	if d.subscriber.EventEnabled(event) {
		d.subscriber.Event(event)
	}
}

// Enter forwards to Subscriber.Enter.
func (d *Dispatch) Enter(span SpanId) { d.subscriber.Enter(span) }

// Exit forwards to Subscriber.Exit.
func (d *Dispatch) Exit(span SpanId) { d.subscriber.Exit(span) }

// CloneSpan forwards to Subscriber.CloneSpan.
func (d *Dispatch) CloneSpan(id SpanId) SpanId { return d.subscriber.CloneSpan(id) }

// DropSpan forwards to Subscriber.DropSpan.
func (d *Dispatch) DropSpan(id SpanId) { d.subscriber.DropSpan(id) }

// TryClose forwards to Subscriber.TryClose.
func (d *Dispatch) TryClose(id SpanId) bool { return d.subscriber.TryClose(id) }

// CurrentSpan forwards to Subscriber.CurrentSpan.
func (d *Dispatch) CurrentSpan() CurrentSpan { return d.subscriber.CurrentSpan() }

// DispatchIs is Dispatch::is: whether the subscriber, or a part of it, is a
// T.
func DispatchIs[T any](d *Dispatch) bool {
	_, ok := DispatchDowncastRef[T](d)
	return ok
}

// DispatchDowncastRef is Dispatch::downcast_ref: the subscriber as a T, or
// the part of it that is a T.
func DispatchDowncastRef[T any](d *Dispatch) (T, bool) {
	if typed, ok := d.subscriber.(T); ok {
		return typed, true
	}
	if parts, ok := d.subscriber.(SubscriberDowncaster); ok {
		if part, ok := parts.DowncastRaw(reflect.TypeFor[T]()); ok {
			typed, ok := part.(T)
			return typed, ok
		}
	}
	var zero T
	return zero, false
}

// FmtDebug is `impl Debug for Dispatch`.
func (d *Dispatch) FmtDebug(f *rs.Formatter) error {
	name := "Dispatch::Scoped"
	if d == globalDispatch.Load() || d.subscriber == Subscriber(noSubscriber) {
		name = "Dispatch::Global"
	}
	address := "0x" + strconv.FormatUint(uint64(uintptr(unsafe.Pointer(d))), 16)
	return f.DebugTuple(name).Field(FieldDisplay(address)).Finish()
}

// ===== impl DefaultGuard =====

// Drop puts back the default dispatcher that was current when the guard was
// made. A second call does nothing.
func (g *DispatcherDefaultGuard) Drop() {
	if g.dropped {
		return
	}
	g.dropped = true
	locals := ThreadLocals()
	if state := loadDispatcherState(locals, false); state != nil {
		state.default_ = g.prior
		state.guards--
		releaseDispatcherState(locals, state)
	}
	scopedCount.Add(-1)
}
