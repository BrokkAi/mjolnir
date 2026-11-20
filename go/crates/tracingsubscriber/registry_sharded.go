// Port of tracing-subscriber 0.3.23 src/registry/sharded.rs

package tracingsubscriber

import (
	"reflect"
	"strconv"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Registry is tracing_subscriber::Registry: a subscriber that stores span
// data and tracks which spans each thread has entered, for layers to look
// up. It records nothing itself.
//
// Rust keeps spans in a sharded slab, so a span's ID is the slot it was put
// in, and IDs of closed spans are reused. Here too a closed span's slot is
// reused (the most recently freed first), but the numbers differ from Rust's
// where spans are made on several threads: the slab's shard is part of
// Rust's ID. The numbers appear in no output.
//
// Per-layer filters (Filtered, FilterState) are not ported, so the
// registry's register_filter and its per-filter span maps are not either.
type Registry struct {
	lock  sync.RWMutex
	spans []*dataInner // the slab: nil is a free slot
	free  []int        // free slots, the last freed last
}

// RegistryData is tracing_subscriber::registry::Data: the data of one span.
type RegistryData struct {
	inner *dataInner
	idx   int
}

// dataInner is DataInner.
type dataInner struct {
	metadata *tracing.Metadata
	parent   rs.Option[tracing.SpanId]
	refCount atomic.Int64
	// The span's `Extensions` typemap.
	extensions extensionsInner
}

// ===== impl Registry =====

// RegistryDefault is `impl Default for Registry`.
func RegistryDefault() *Registry {
	return &Registry{}
}

func idxToId(idx int) tracing.SpanId {
	return tracing.SpanIdFromU64(uint64(idx) + 1)
}

func idToIdx(id tracing.SpanId) int {
	return int(id.IntoU64() - 1)
}

// registryStackKey is the key of a registry's span stack in thread-local
// storage: Registry::current_spans.
type registryStackKey struct {
	registry *Registry
}

// closeCountKey is the key of CLOSE_COUNT in thread-local storage.
type closeCountKey struct{}

// closeGuard is CloseGuard.
type closeGuard struct {
	id        tracing.SpanId
	registry  *Registry
	isClosing bool
}

func (r *Registry) get(id tracing.SpanId) rs.Option[*dataInner] {
	idx := idToIdx(id)
	r.lock.RLock()
	defer r.lock.RUnlock()
	if idx < 0 || idx >= len(r.spans) || r.spans[idx] == nil {
		return rs.None[*dataInner]()
	}
	return rs.Some(r.spans[idx])
}

// startClose is Registry::start_close: it notes that a close is in
// progress on this thread, so that the span's data stays until every layer
// has seen it close.
func (r *Registry) startClose(id tracing.SpanId) *closeGuard {
	locals := tracing.ThreadLocals()
	count := 0
	if stored, ok := locals.Get(closeCountKey{}); ok {
		count = stored.(int)
	}
	locals.Set(closeCountKey{}, count+1)
	return &closeGuard{id: id, registry: r, isClosing: false}
}

// spanStack is the calling thread's stack of entered spans, made when
// create says so.
func (r *Registry) spanStack(locals tracing.LocalStore, create bool) *spanStack {
	if stored, ok := locals.Get(registryStackKey{registry: r}); ok {
		return stored.(*spanStack)
	}
	if !create {
		return nil
	}
	stack := &spanStack{}
	locals.Set(registryStackKey{registry: r}, stack)
	return stack
}

// releaseSpanStack forgets an empty stack, so that a goroutine that has left
// every span it entered keeps nothing in thread-local storage.
func (r *Registry) releaseSpanStack(locals tracing.LocalStore, stack *spanStack) {
	if len(stack.stack) == 0 {
		locals.Delete(registryStackKey{registry: r})
	}
}

// OnRegisterDispatch is the default Subscriber::on_register_dispatch.
func (r *Registry) OnRegisterDispatch(subscriber *tracing.Dispatch) {}

// RegisterCallsite is `impl Subscriber for Registry`: always, there being
// no per-layer filters.
func (r *Registry) RegisterCallsite(metadata *tracing.Metadata) tracing.SubscriberInterest {
	return tracing.SubscriberInterestAlways()
}

// Enabled is `impl Subscriber for Registry`.
func (r *Registry) Enabled(metadata *tracing.Metadata) bool {
	return true
}

// MaxLevelHint is the default Subscriber::max_level_hint.
func (r *Registry) MaxLevelHint() rs.Option[tracing.LevelFilter] {
	return tracing.SubscriberDefaultMaxLevelHint(r)
}

// NewSpan is `impl Subscriber for Registry`: it stores the span with one
// reference, and takes a reference on its parent, which stays open until
// its children have closed.
func (r *Registry) NewSpan(attrs *tracing.SpanAttributes) tracing.SpanId {
	parent := rs.None[tracing.SpanId]()
	if attrs.IsRoot() {
		// None
	} else if attrs.IsContextual() {
		if id, ok := r.CurrentSpan().Id().Get(); ok {
			parent = rs.Some(r.CloneSpan(id))
		}
	} else if id, ok := attrs.Parent().Get(); ok {
		parent = rs.Some(r.CloneSpan(id))
	}

	data := &dataInner{metadata: attrs.Metadata(), parent: parent}
	data.refCount.Store(1)

	r.lock.Lock()
	defer r.lock.Unlock()
	var idx int
	if n := len(r.free); n > 0 {
		idx = r.free[n-1]
		r.free = r.free[:n-1]
		r.spans[idx] = data
	} else {
		idx = len(r.spans)
		r.spans = append(r.spans, data)
	}
	return idxToId(idx)
}

// Record is `impl Subscriber for Registry`: nothing.
func (r *Registry) Record(span tracing.SpanId, values *tracing.SpanRecord) {}

// RecordFollowsFrom is `impl Subscriber for Registry`: nothing.
func (r *Registry) RecordFollowsFrom(span tracing.SpanId, follows tracing.SpanId) {}

// EventEnabled is `impl Subscriber for Registry`.
func (r *Registry) EventEnabled(event *tracing.Event) bool {
	return true
}

// Event is `impl Subscriber for Registry`: nothing.
func (r *Registry) Event(event *tracing.Event) {}

// Enter is `impl Subscriber for Registry`: the span goes on the calling
// thread's stack.
func (r *Registry) Enter(id tracing.SpanId) {
	r.spanStack(tracing.ThreadLocals(), true).push(id)
}

// Exit is `impl Subscriber for Registry`: the span comes off the calling
// thread's stack.
func (r *Registry) Exit(id tracing.SpanId) {
	locals := tracing.ThreadLocals()
	if spans := r.spanStack(locals, false); spans != nil {
		spans.pop(id)
		r.releaseSpanStack(locals, spans)
	}
}

// CloneSpan is `impl Subscriber for Registry`: one more reference.
func (r *Registry) CloneSpan(id tracing.SpanId) tracing.SpanId {
	span, ok := r.get(id).Get()
	if !ok {
		rs.PanicAt(1, "tried to clone "+spanIdDebug(id)+", but no span exists with that ID\n"+
			"This may be caused by consuming a parent span (`parent: span`) rather than borrowing it (`parent: &span`).")
	}
	// Like `std::sync::Arc`, adds to the ref count (on clone) don't require
	// a strong ordering; if we call` clone_span`, the reference count must
	// always at least 1. The only synchronization necessary is between
	// calls to `try_close`: we have to ensure that all threads have
	// dropped their refs to the span before the span is closed.
	refs := span.refCount.Add(1) - 1
	if refs == 0 {
		rs.PanicAt(1, "assertion `left != right` failed: tried to clone a span ("+spanIdDebug(id)+") that already closed\n  left: 0\n right: 0")
	}
	return id
}

// CurrentSpan is `impl Subscriber for Registry`: the innermost span the
// calling thread has entered.
func (r *Registry) CurrentSpan() tracing.CurrentSpan {
	spans := r.spanStack(tracing.ThreadLocals(), false)
	if spans == nil {
		return tracing.CurrentSpanNone()
	}
	id, ok := spans.current().Get()
	if !ok {
		return tracing.CurrentSpanNone()
	}
	span, ok := r.get(id).Get()
	if !ok {
		return tracing.CurrentSpanNone()
	}
	return tracing.CurrentSpanNew(id, span.metadata)
}

// DropSpan is the default Subscriber::drop_span.
func (r *Registry) DropSpan(id tracing.SpanId) { tracing.SubscriberDefaultDropSpan(r, id) }

// TryClose is `impl Subscriber for Registry`: one reference less, and true
// when it was the last. The span's data is removed by the close guard of
// the Layered that asked, once every layer has seen the close.
//
// Rust does not panic for an unknown span while the thread is already
// panicking; Go cannot tell, so it always does.
func (r *Registry) TryClose(id tracing.SpanId) bool {
	span, ok := r.get(id).Get()
	if !ok {
		rs.PanicAt(1, "tried to drop a ref to "+spanIdDebug(id)+", but no such span exists!")
	}

	refs := span.refCount.Add(-1) + 1
	rs.Assert(refs < 1<<62, "reference count overflow!")
	if refs > 1 {
		return false
	}

	// Synchronize if we are actually removing the span (stolen
	// from std::Arc); this ensures that all other `try_close` calls on
	// other threads happen-before we actually remove the span.
	return true
}

// DowncastRaw is Subscriber::downcast_raw: the registry itself.
func (r *Registry) DowncastRaw(target reflect.Type) (any, bool) {
	if target == reflect.TypeFor[*Registry]() || target == reflect.TypeFor[Registry]() {
		return r, true
	}
	return nil, false
}

// SpanData is `impl LookupSpan for Registry`.
func (r *Registry) SpanData(id tracing.SpanId) rs.Option[*RegistryData] {
	inner, ok := r.get(id).Get()
	if !ok {
		return rs.None[*RegistryData]()
	}
	return rs.Some(&RegistryData{inner: inner, idx: idToIdx(id)})
}

// clear is `Pool::clear` for one slot: DataInner::clear, then the slot is
// free again.
func (r *Registry) clear(idx int) {
	r.lock.RLock()
	var data *dataInner
	if idx >= 0 && idx < len(r.spans) {
		data = r.spans[idx]
	}
	r.lock.RUnlock()
	if data == nil {
		return
	}
	data.clear()
	r.lock.Lock()
	defer r.lock.Unlock()
	if r.spans[idx] == data {
		r.spans[idx] = nil
		r.free = append(r.free, idx)
	}
}

// ===== impl CloseGuard =====

func (g *closeGuard) setClosing() {
	g.isClosing = true
}

// drop is `impl Drop for CloseGuard`.
func (g *closeGuard) drop() {
	locals := tracing.ThreadLocals()
	c := 0
	if stored, ok := locals.Get(closeCountKey{}); ok {
		c = stored.(int)
	}
	// Decrement the count to indicate that _this_ guard's
	// `on_close` callback has completed.
	//
	// Note that we *must* do this before we actually remove the span
	// from the registry, since dropping the `DataInner` may trigger a
	// new close, if this span is the last reference to a parent span.
	if c-1 == 0 {
		locals.Delete(closeCountKey{})
	} else {
		locals.Set(closeCountKey{}, c-1)
	}

	// If the current close count is 1, this stack frame is the last
	// `on_close` call. If the span is closing, it's okay to remove the
	// span.
	if c == 1 && g.isClosing {
		g.registry.clear(idToIdx(g.id))
	}
}

// ===== impl Data =====

// Id is SpanData::id.
func (d *RegistryData) Id() tracing.SpanId { return idxToId(d.idx) }

// Metadata is SpanData::metadata.
func (d *RegistryData) Metadata() *tracing.Metadata { return d.inner.metadata }

// Parent is SpanData::parent.
func (d *RegistryData) Parent() rs.Option[tracing.SpanId] { return d.inner.parent }

// Extensions is SpanData::extensions.
func (d *RegistryData) Extensions() *RegistryExtensions { return d.inner.extensions.read() }

// ExtensionsMut is SpanData::extensions_mut.
func (d *RegistryData) ExtensionsMut() *RegistryExtensionsMut { return d.inner.extensions.write() }

// ===== impl DataInner =====

// clear is `impl Clear for DataInner`.
func (d *dataInner) clear() {
	// A span is not considered closed until all of its children have closed.
	// Therefore, each span's `DataInner` holds a "reference" to the parent
	// span, keeping the parent span open until all its children have closed.
	// When we close a span, we must then decrement the parent's ref count
	// (potentially, allowing it to close, if this child is the last reference
	// to that span).
	if d.parent.IsSome() {
		// Note that --- because `Layered::try_close` works by calling
		// `try_close` on the inner subscriber and using the return value to
		// determine whether to call the `Layer`'s `on_close` callback ---
		// we must call `try_close` on the entire subscriber stack, rather
		// than just on the registry. If the registry called `try_close` on
		// itself directly, the layers wouldn't see the close notification.
		subscriber := tracing.DispatcherGetDefault((*tracing.Dispatch).Clone)
		if parent, ok := d.parent.Take().Get(); ok {
			_ = subscriber.TryClose(parent)
		}
	}

	// Clear (but do not deallocate!) the pooled `HashMap` for the span's extensions.
	d.extensions.clear()
}

// spanIdDebug is `format!("{:?}", id)` for a span ID: `Id(5)`.
func spanIdDebug(id tracing.SpanId) string {
	return "Id(" + strconv.FormatUint(id.IntoU64(), 10) + ")"
}
