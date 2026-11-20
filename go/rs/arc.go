// Port of alloc 1.99.1 src/sync.rs (Arc and Weak)

package rs

import (
	"os"
	"reflect"
	"runtime"
	"strconv"
	"strings"
	"sync/atomic"
	"unsafe"
)

// Arc is Rust's Arc<T> (and Rc<T>) for the types whose reference count the
// Rust code can observe: it downgrades them to Weak, reads
// `Arc::strong_count`, or takes the value back with `Arc::try_unwrap`,
// `Arc::get_mut` or `Arc::make_mut` on a counted value. Every other Arc<T> is
// a plain *T (translation-rules.md, rule 4).
//
// The counting is Rust's, operation for operation: a strong count, and a weak
// count that holds one for all the strong handles together.
//
// An Arc value is one handle, as one Arc is in Rust:
//
//   - Clone makes another handle. Copying the Go value does not: an
//     assignment is a Rust move, and both copies are the same handle.
//   - Drop gives the handle up. It is idempotent, and it does nothing on the
//     zero Arc. When the last handle goes, the value's own Drop method runs
//     (Dropper), on the goroutine that dropped it.
//   - A handle that was dropped, or taken by ArcTryUnwrap, ArcIntoInner or
//     ArcUnwrapOrClone, cannot be used again: Get, Clone and the functions
//     below panic on it, and on the zero Arc. Rust rejects such code when it
//     compiles; here it is a mistake in the port.
//
// Go has no destructors, so a handle that the code forgets to drop keeps its
// count. As a safety net, a handle the garbage collector finds unreachable
// releases its count then, some time later and on another goroutine; if that
// was the last handle, the value's Drop runs there, and a panic in it goes to
// the panic hook instead of ending the program. Correct code never relies on
// this: a count that is read (`strong_count`, `get_mut`, `Weak::upgrade`) is
// only right when every handle is dropped where Rust drops it. Set
// MJ_RS_LEAK_CHECK=1, or call SetArcLeakCheck, to have every handle that
// reaches the safety net reported with the place that created it.
//
// The pointer Get returns does not keep the handle alive. Keep the handle
// (in a variable with a deferred Drop, or in a struct) for as long as the
// pointer is used.
//
// Clone, Eq and Cmp treat an Arc as Rust does: a clone is another handle to
// the same value, and two Arcs are compared by the values they hold.
type Arc[T any] struct {
	handle *arcHandle[T]
}

// Weak is Rust's Weak<T>: a handle that does not keep the value alive and
// must be upgraded to an Arc to reach it. The zero Weak is `Weak::new()`: it
// points to nothing and never upgrades. Like an Arc, a Weak is one handle
// with its own Clone and Drop; it counts in `Arc::weak_count`, and while it
// exists `Arc::get_mut` answers None.
type Weak[T any] struct {
	handle *weakHandle[T]
}

// Dropper is a value with a destructor (`impl Drop`). The value an Arc holds
// is dropped through it when the last handle goes.
type Dropper interface {
	Drop()
}

// arcInner is the shared allocation, Rust's ArcInner.
type arcInner[T any] struct {
	// strong counts the Arc handles. Once it is zero it stays zero.
	strong atomic.Uintptr
	// weak counts the Weak handles, plus one held by all the Arc handles
	// together. arcWeakLocked in it is the lock ArcGetMut takes.
	weak atomic.Uintptr
	// data is the value: nil once it was dropped or moved out, so that Weak
	// handles do not keep it in memory.
	data atomic.Pointer[T]
}

const arcWeakLocked = ^uintptr(0)

// arcHandle is the identity of one Arc handle. It is a heap object of its
// own so that Drop can be idempotent across copies of the Arc value, and so
// that the garbage collector can tell when the handle is gone.
type arcHandle[T any] struct {
	inner *arcInner[T]
	// gone is set once the handle was dropped or its value taken.
	gone    atomic.Bool
	cleanup runtime.Cleanup
}

type weakHandle[T any] struct {
	inner   *arcInner[T]
	gone    atomic.Bool
	cleanup runtime.Cleanup
}

// arcRelease is what the garbage collector's cleanup of a handle needs. It
// must not refer to the handle.
type arcRelease[T any] struct {
	inner *arcInner[T]
	weak  bool
	// site is where the handle was created, recorded only while the leak
	// check is on.
	site *arcSite
}

type arcSite struct {
	file     string
	line     int
	function string
}

// ArcNew is `Arc::new(value)`.
func ArcNew[T any](value T) Arc[T] {
	data := new(T)
	*data = value
	return newArc(data)
}

// ArcFromBox is `Arc::from(box)`: the Arc takes over the value box points
// to, without copying it. The caller must not use box afterwards except
// through the Arc.
func ArcFromBox[T any](box *T) Arc[T] {
	if box == nil {
		PanicAt(1, "rs.ArcFromBox: the box is nil")
	}
	return newArc(box)
}

func newArc[T any](data *T) Arc[T] {
	inner := &arcInner[T]{}
	inner.strong.Store(1)
	inner.weak.Store(1)
	inner.data.Store(data)
	return Arc[T]{handle: newArcHandle(inner)}
}

func newArcHandle[T any](inner *arcInner[T]) *arcHandle[T] {
	handle := &arcHandle[T]{inner: inner}
	handle.cleanup = runtime.AddCleanup(handle, releaseLeakedHandle[T], arcRelease[T]{inner: inner, site: captureArcSite()})
	return handle
}

func newWeakHandle[T any](inner *arcInner[T]) *weakHandle[T] {
	handle := &weakHandle[T]{inner: inner}
	handle.cleanup = runtime.AddCleanup(handle, releaseLeakedHandle[T], arcRelease[T]{inner: inner, weak: true, site: captureArcSite()})
	return handle
}

// live returns the handle, or panics when the Arc is the zero value or was
// dropped.
func (a Arc[T]) live(operation string) *arcHandle[T] {
	handle := a.handle
	if handle == nil {
		PanicAt(2, "rs.Arc: "+operation+" on the zero Arc, which holds no value")
	}
	if handle.gone.Load() {
		PanicAt(2, "rs.Arc: "+operation+" on a handle that was already dropped")
	}
	return handle
}

// live returns the handle, nil for `Weak::new()`, or panics when the Weak was
// dropped.
func (w Weak[T]) live(operation string) *weakHandle[T] {
	handle := w.handle
	if handle != nil && handle.gone.Load() {
		PanicAt(2, "rs.Weak: "+operation+" on a handle that was already dropped")
	}
	return handle
}

// Get returns a pointer to the value: it is the dereference of an Arc
// (`*arc`, `arc.field`, `Arc::as_ref`). The value is shared, so write
// through the pointer only as Rust would, through the value's own locks or
// after ArcGetMut or ArcMakeMut.
func (a Arc[T]) Get() *T {
	return a.live("Get").inner.data.Load()
}

// Clone is `Arc::clone(&a)`: another handle to the same value.
func (a Arc[T]) Clone() Arc[T] {
	handle := a.live("Clone")
	handle.inner.strong.Add(1)
	clone := Arc[T]{handle: newArcHandle(handle.inner)}
	runtime.KeepAlive(handle)
	return clone
}

// Drop is the end of the handle's life. When it was the last one, the value
// is dropped: its Drop method runs if it has one, and a panic in it reaches
// the caller.
func (a *Arc[T]) Drop() {
	handle := a.handle
	if handle == nil || !handle.gone.CompareAndSwap(false, true) {
		return
	}
	handle.cleanup.Stop()
	handle.inner.releaseStrong()
	runtime.KeepAlive(handle)
}

// Forget is `mem::forget(a)`: the handle is given up without releasing its
// count, so the value is never dropped.
func (a Arc[T]) Forget() {
	handle := a.live("Forget")
	handle.gone.Store(true)
	handle.cleanup.Stop()
	runtime.KeepAlive(handle)
}

// releaseStrong is Arc's Drop: one strong handle less, and the value is
// dropped with the last.
func (inner *arcInner[T]) releaseStrong() {
	if inner.strong.Add(^uintptr(0)) != 0 {
		return
	}
	// The weak count that the strong handles held together goes even when
	// the value's destructor panics.
	defer inner.releaseWeak()
	dropArcValue(inner.data.Swap(nil))
}

// releaseWeak is Weak's Drop. Rust frees the allocation at zero; here the
// garbage collector does.
func (inner *arcInner[T]) releaseWeak() {
	inner.weak.Add(^uintptr(0))
}

// dropArcValue runs the destructor of the value an Arc held.
func dropArcValue[T any](data *T) {
	if data == nil {
		return
	}
	if dropper, ok := any(data).(Dropper); ok {
		dropper.Drop()
		return
	}
	// An Arc of an interface or of a pointer holds a value whose own type
	// has the destructor.
	switch reflect.TypeFor[T]().Kind() {
	case reflect.Pointer:
		if *(*unsafe.Pointer)(unsafe.Pointer(data)) == nil {
			return
		}
		fallthrough
	case reflect.Interface:
		if dropper, ok := any(*data).(Dropper); ok {
			dropper.Drop()
		}
	}
}

// ArcDowngrade is `Arc::downgrade(&a)`.
func ArcDowngrade[T any](a Arc[T]) Weak[T] {
	handle := a.live("ArcDowngrade")
	inner := handle.inner
	for {
		current := inner.weak.Load()
		if current == arcWeakLocked {
			// ArcGetMut is looking at the counts; it is quick.
			runtime.Gosched()
			continue
		}
		if inner.weak.CompareAndSwap(current, current+1) {
			break
		}
	}
	weak := Weak[T]{handle: newWeakHandle(inner)}
	runtime.KeepAlive(handle)
	return weak
}

// ArcStrongCount is `Arc::strong_count(&a)`.
func ArcStrongCount[T any](a Arc[T]) int {
	handle := a.live("ArcStrongCount")
	count := handle.inner.strong.Load()
	runtime.KeepAlive(handle)
	return int(count)
}

// ArcWeakCount is `Arc::weak_count(&a)`: the number of Weak handles.
func ArcWeakCount[T any](a Arc[T]) int {
	handle := a.live("ArcWeakCount")
	count := handle.inner.weak.Load()
	runtime.KeepAlive(handle)
	if count == arcWeakLocked {
		// The count was 1, the strong handles' own, just before the lock.
		return 0
	}
	return int(count - 1)
}

// ArcTryUnwrap is `Arc::try_unwrap(a)`. With exactly one strong handle it
// returns the value and true, and the handle is used up; the value is moved
// out, not dropped, and Weak handles stop upgrading. Otherwise it returns
// false and the handle is still the caller's, as `Err(a)` hands it back.
func ArcTryUnwrap[T any](a Arc[T]) (T, bool) {
	handle := a.live("ArcTryUnwrap")
	inner := handle.inner
	if !inner.strong.CompareAndSwap(1, 0) {
		runtime.KeepAlive(handle)
		var none T
		return none, false
	}
	handle.gone.Store(true)
	handle.cleanup.Stop()
	value := *inner.data.Swap(nil)
	inner.releaseWeak()
	runtime.KeepAlive(handle)
	return value, true
}

// ArcIntoInner is `Arc::into_inner(a)`: the handle is given up, and the value
// is returned when it was the last strong one. When every handle is given up
// this way, exactly one call gets the value.
func ArcIntoInner[T any](a Arc[T]) Option[T] {
	handle := a.live("ArcIntoInner")
	handle.gone.Store(true)
	handle.cleanup.Stop()
	inner := handle.inner
	if inner.strong.Add(^uintptr(0)) != 0 {
		runtime.KeepAlive(handle)
		return None[T]()
	}
	value := *inner.data.Swap(nil)
	inner.releaseWeak()
	runtime.KeepAlive(handle)
	return Some(value)
}

// ArcUnwrapOrClone is `Arc::unwrap_or_clone(a)`: the value itself when a is
// the only strong handle, a clone of it otherwise. The handle is used up
// either way.
func ArcUnwrapOrClone[T any](a Arc[T]) T {
	if value, ok := ArcTryUnwrap(a); ok {
		return value
	}
	value := cloneValue(*a.Get())
	a.Drop()
	return value
}

// ArcGetMut is `Arc::get_mut(&mut a)`: a pointer through which the value may
// be changed, when no other Arc or Weak handle to it exists.
func ArcGetMut[T any](a Arc[T]) Option[*T] {
	handle := a.live("ArcGetMut")
	unique := handle.inner.isUnique()
	data := handle.inner.data.Load()
	runtime.KeepAlive(handle)
	if !unique {
		return None[*T]()
	}
	return Some(data)
}

// isUnique is Arc::is_unique: no other strong and no weak handle. The weak
// count is locked while the strong count is read, so that no Weak can be
// made and upgraded in between.
func (inner *arcInner[T]) isUnique() bool {
	if !inner.weak.CompareAndSwap(1, arcWeakLocked) {
		return false
	}
	unique := inner.strong.Load() == 1
	inner.weak.Store(1)
	return unique
}

// ArcMakeMut is `Arc::make_mut(&mut a)`: a pointer through which the value
// may be changed. When other strong handles exist the value is cloned first
// and *a becomes a handle to the clone; when only Weak handles exist the
// value moves to a new allocation and they stop upgrading.
func ArcMakeMut[T any](a *Arc[T]) *T {
	handle := a.live("ArcMakeMut")
	inner := handle.inner
	switch {
	case !inner.strong.CompareAndSwap(1, 0):
		// Another strong pointer exists, so we must clone.
		cloned := cloneValue(*inner.data.Load())
		previous := *a
		*a = newArc(&cloned)
		previous.Drop()
	case inner.weak.Load() != 1:
		// We removed the last strong ref, but there are additional weak
		// refs remaining. We'll move the contents to a new Arc, and
		// invalidate the other weak refs.
		handle.gone.Store(true)
		handle.cleanup.Stop()
		*a = newArc(inner.data.Swap(nil))
		inner.releaseWeak()
	default:
		// We were the sole reference of either kind; bump back up the
		// strong ref count.
		inner.strong.Store(1)
	}
	runtime.KeepAlive(handle)
	return a.handle.inner.data.Load()
}

// ArcPtrEq is `Arc::ptr_eq(&a, &b)`: whether the two handles share one
// value.
func ArcPtrEq[T any](a, b Arc[T]) bool {
	return a.live("ArcPtrEq").inner == b.live("ArcPtrEq").inner
}

// ArcAsPtr is `Arc::as_ptr(&a)`: the address of the value, for use as an
// identity.
func ArcAsPtr[T any](a Arc[T]) *T {
	return a.live("ArcAsPtr").inner.data.Load()
}

// WeakNew is `Weak::new()`: a Weak that points to nothing.
func WeakNew[T any]() Weak[T] {
	return Weak[T]{}
}

// Upgrade is `weak.upgrade()`: a new Arc handle, or None once the value was
// dropped or moved out.
func (w Weak[T]) Upgrade() Option[Arc[T]] {
	handle := w.live("Upgrade")
	if handle == nil {
		return None[Arc[T]]()
	}
	inner := handle.inner
	for {
		// This never takes the count from zero to one.
		current := inner.strong.Load()
		if current == 0 {
			runtime.KeepAlive(handle)
			return None[Arc[T]]()
		}
		if inner.strong.CompareAndSwap(current, current+1) {
			upgraded := Arc[T]{handle: newArcHandle(inner)}
			runtime.KeepAlive(handle)
			return Some(upgraded)
		}
	}
}

// Clone is `Weak::clone(&w)`: another Weak handle.
func (w Weak[T]) Clone() Weak[T] {
	handle := w.live("Clone")
	if handle == nil {
		return Weak[T]{}
	}
	handle.inner.weak.Add(1)
	clone := Weak[T]{handle: newWeakHandle(handle.inner)}
	runtime.KeepAlive(handle)
	return clone
}

// Drop is the end of the Weak handle's life. It is idempotent.
func (w *Weak[T]) Drop() {
	handle := w.handle
	if handle == nil || !handle.gone.CompareAndSwap(false, true) {
		return
	}
	handle.cleanup.Stop()
	handle.inner.releaseWeak()
	runtime.KeepAlive(handle)
}

// Forget is `mem::forget(w)`: the handle is given up without releasing its
// count.
func (w Weak[T]) Forget() {
	handle := w.live("Forget")
	if handle == nil {
		return
	}
	handle.gone.Store(true)
	handle.cleanup.Stop()
	runtime.KeepAlive(handle)
}

// StrongCount is `weak.strong_count()`: the number of Arc handles, 0 for
// `Weak::new()`.
func (w Weak[T]) StrongCount() int {
	handle := w.live("StrongCount")
	if handle == nil {
		return 0
	}
	count := handle.inner.strong.Load()
	runtime.KeepAlive(handle)
	return int(count)
}

// WeakCount is `weak.weak_count()`: the number of Weak handles, or 0 when no
// Arc handle remains.
func (w Weak[T]) WeakCount() int {
	handle := w.live("WeakCount")
	if handle == nil {
		return 0
	}
	weak := handle.inner.weak.Load()
	strong := handle.inner.strong.Load()
	runtime.KeepAlive(handle)
	if strong == 0 {
		return 0
	}
	// A strong handle was there after the weak count was read, so the count
	// held for the strong handles was in it.
	return int(weak - 1)
}

// PtrEq is `weak.ptr_eq(&other)`: whether both point to the same value, or
// both to nothing.
func (w Weak[T]) PtrEq(other Weak[T]) bool {
	return WeakPtrEq(w, other)
}

// WeakPtrEq is `Weak::ptr_eq(&a, &b)`.
func WeakPtrEq[T any](a, b Weak[T]) bool {
	var left, right *arcInner[T]
	if handle := a.live("WeakPtrEq"); handle != nil {
		left = handle.inner
	}
	if handle := b.live("WeakPtrEq"); handle != nil {
		right = handle.inner
	}
	return left == right
}

// Equal is Arc's PartialEq: the values are compared, not the handles.
func (a Arc[T]) Equal(other Arc[T]) bool {
	return shapeOf(reflect.TypeFor[T]()).equalPointees(unsafe.Pointer(a.Get()), unsafe.Pointer(other.Get()))
}

// Compare is Arc's Ord: the order of the values.
func (a Arc[T]) Compare(other Arc[T]) int {
	order, _ := shapeOf(reflect.TypeFor[T]()).comparePointees(unsafe.Pointer(a.Get()), unsafe.Pointer(other.Get()))
	return order
}

// FmtDisplay is Arc's Display: that of the value.
func (a Arc[T]) FmtDisplay(f *Formatter) error {
	return DisplayFmt(a.Get(), f)
}

// FmtDebug is Arc's Debug: that of the value.
func (a Arc[T]) FmtDebug(f *Formatter) error {
	return DebugFmt(a.Get(), f)
}

// FmtDebug is Weak's Debug, which shows nothing of the value.
func (w Weak[T]) FmtDebug(f *Formatter) error {
	return f.WriteStr("(Weak)")
}

// ReflectSetDefault is Arc's Default: `Arc::new(T::default())`.
func (a *Arc[T]) ReflectSetDefault() {
	*a = ArcNew(defaultValue[T]())
}

// ReflectArc lets code that works by reflection, such as go/crates/serde,
// see through an Arc, whose fields are private. *Arc[T] implements it.
type ReflectArc interface {
	// ReflectArcType returns the type T.
	ReflectArcType() reflect.Type
	// ReflectArcGet returns the pointer to the value (a *T), as Get does.
	ReflectArcGet() any
	// ReflectArcSet stores `Arc::new(value)` in the receiver, value being a
	// T. A handle the receiver held is dropped.
	ReflectArcSet(value any)
}

// ReflectArcType implements ReflectArc.
func (a *Arc[T]) ReflectArcType() reflect.Type { return reflect.TypeFor[T]() }

// ReflectArcGet implements ReflectArc.
func (a *Arc[T]) ReflectArcGet() any { return a.Get() }

// ReflectArcSet implements ReflectArc.
func (a *Arc[T]) ReflectArcSet(value any) {
	fresh := ArcNew(value.(T))
	a.Drop()
	*a = fresh
}

// ArcLeak describes a handle that the garbage collector found unreachable
// before it was dropped.
type ArcLeak struct {
	// Handle is "Arc" or "Weak".
	Handle string
	// Type is the Go type of the value the handle points to.
	Type string
	// File, Line and Function say where the handle was created: the first
	// caller outside this package.
	File     string
	Line     int
	Function string
}

// String describes the leak in one line.
func (l ArcLeak) String() string {
	return "rs." + l.Handle + "[" + l.Type + "] created at " + l.File + ":" + strconv.Itoa(l.Line) +
		" (" + l.Function + ") was garbage collected without Drop"
}

var (
	arcLeakCheck atomic.Bool
	arcLeakHook  atomic.Pointer[func(ArcLeak)]
)

func init() {
	arcLeakCheck.Store(os.Getenv("MJ_RS_LEAK_CHECK") == "1")
}

// SetArcLeakCheck turns the leak check on or off and returns what it was. It
// starts on when the environment variable MJ_RS_LEAK_CHECK is 1. While it is
// on, every new Arc and Weak handle records where it was created, and a
// handle that is garbage collected without Drop is reported to the hook.
func SetArcLeakCheck(on bool) bool {
	return arcLeakCheck.Swap(on)
}

// SetArcLeakHook installs the function that receives leak reports and
// returns the previous one. nil stands for the default, which writes each
// report to standard error. The hook runs on a goroutine of the garbage
// collector, some time after the handle became unreachable.
func SetArcLeakHook(hook func(ArcLeak)) func(ArcLeak) {
	var installed *func(ArcLeak)
	if hook != nil {
		installed = &hook
	}
	if previous := arcLeakHook.Swap(installed); previous != nil {
		return *previous
	}
	return nil
}

// captureArcSite returns where the handle being made was asked for, when the
// leak check is on.
func captureArcSite() *arcSite {
	if !arcLeakCheck.Load() {
		return nil
	}
	var callers [48]uintptr
	frames := runtime.CallersFrames(callers[:runtime.Callers(2, callers[:])])
	for {
		frame, more := frames.Next()
		if !isArcInternalFrame(frame) {
			return &arcSite{file: frame.File, line: frame.Line, function: frame.Function}
		}
		if !more {
			return &arcSite{}
		}
	}
}

// isArcInternalFrame reports whether a stack frame belongs to this package
// (its tests apart), or to the reflection that Clone and Default call a
// handle's methods through.
func isArcInternalFrame(frame runtime.Frame) bool {
	if strings.HasPrefix(frame.Function, "reflect.") || strings.HasPrefix(frame.Function, "runtime.") {
		return true
	}
	return strings.HasPrefix(frame.Function, rsPkgPath+".") && !strings.HasSuffix(frame.File, "_test.go")
}

// releaseLeakedHandle is the safety net: the garbage collector calls it for
// a handle that became unreachable without Drop.
func releaseLeakedHandle[T any](release arcRelease[T]) {
	if release.site != nil && arcLeakCheck.Load() {
		leak := ArcLeak{
			Handle:   "Arc",
			Type:     reflect.TypeFor[T]().String(),
			File:     release.site.file,
			Line:     release.site.line,
			Function: release.site.function,
		}
		if release.weak {
			leak.Handle = "Weak"
		}
		if hook := arcLeakHook.Load(); hook != nil {
			(*hook)(leak)
		} else {
			os.Stderr.WriteString("rs: leak: " + leak.String() + "\n")
		}
	}
	if release.weak {
		release.inner.releaseWeak()
		return
	}
	if release.inner.strong.Add(^uintptr(0)) != 0 {
		return
	}
	// The last handle: the value's destructor may block, and must not hold
	// up the collector's other cleanups.
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				RunPanicHook(PanicInfoOf(recovered), "rs-arc-cleanup")
			}
		}()
		defer release.inner.releaseWeak()
		dropArcValue(release.inner.data.Swap(nil))
	}()
}
