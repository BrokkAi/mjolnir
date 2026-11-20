// Port of core 1.99.1 src/sync/atomic.rs

package rssync

import (
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Ordering is std::sync::atomic::Ordering: the memory ordering an atomic
// operation asks for.
//
// Go's atomic operations are all sequentially consistent, which is at least
// as strong as every ordering, so the port accepts an Ordering and does not
// otherwise use it. It does check it where Rust does: a load cannot be
// Release or AcqRel, a store cannot be Acquire or AcqRel, and the failure
// ordering of a compare-exchange cannot be Release or AcqRel. Those panic
// with Rust's text.
type Ordering uint8

const (
	// Ordering_Relaxed is Ordering::Relaxed.
	Ordering_Relaxed Ordering = iota
	// Ordering_Release is Ordering::Release.
	Ordering_Release
	// Ordering_Acquire is Ordering::Acquire.
	Ordering_Acquire
	// Ordering_AcqRel is Ordering::AcqRel.
	Ordering_AcqRel
	// Ordering_SeqCst is Ordering::SeqCst.
	Ordering_SeqCst
)

var _ = rs.RegisterEnum[Ordering]("Ordering",
	rs.Const(Ordering_Relaxed, "Relaxed"),
	rs.Const(Ordering_Release, "Release"),
	rs.Const(Ordering_Acquire, "Acquire"),
	rs.Const(Ordering_AcqRel, "AcqRel"),
	rs.Const(Ordering_SeqCst, "SeqCst"),
)

// String is the variant's name, which is also its derived Debug output.
func (o Ordering) String() string {
	switch o {
	case Ordering_Relaxed:
		return "Relaxed"
	case Ordering_Release:
		return "Release"
	case Ordering_Acquire:
		return "Acquire"
	case Ordering_AcqRel:
		return "AcqRel"
	case Ordering_SeqCst:
		return "SeqCst"
	default:
		panic(rs.Unreachable("rssync: invalid Ordering {}", int(o)))
	}
}

// FmtDebug is the derived Debug impl: the variant's name.
func (o Ordering) FmtDebug(f *rs.Formatter) error { return f.WriteStr(o.String()) }

// checkLoad is atomic_load's match on the ordering, attributed to the caller
// skip frames above the caller of checkLoad.
func checkLoad(order Ordering, skip int) {
	switch order {
	case Ordering_Relaxed, Ordering_Acquire, Ordering_SeqCst:
	case Ordering_Release:
		rs.PanicAt(skip+1, "there is no such thing as a release load")
	case Ordering_AcqRel:
		rs.PanicAt(skip+1, "there is no such thing as an acquire-release load")
	default:
		panic(rs.Unreachable("rssync: invalid Ordering {}", int(order)))
	}
}

// checkStore is atomic_store's match on the ordering.
func checkStore(order Ordering, skip int) {
	switch order {
	case Ordering_Relaxed, Ordering_Release, Ordering_SeqCst:
	case Ordering_Acquire:
		rs.PanicAt(skip+1, "there is no such thing as an acquire store")
	case Ordering_AcqRel:
		rs.PanicAt(skip+1, "there is no such thing as an acquire-release store")
	default:
		panic(rs.Unreachable("rssync: invalid Ordering {}", int(order)))
	}
}

// checkCompareExchange is atomic_compare_exchange's match on the success and
// failure orderings: any success ordering goes with a failure ordering that
// is a load ordering.
func checkCompareExchange(success, failure Ordering, skip int) {
	if success > Ordering_SeqCst {
		panic(rs.Unreachable("rssync: invalid Ordering {}", int(success)))
	}
	switch failure {
	case Ordering_Relaxed, Ordering_Acquire, Ordering_SeqCst:
	case Ordering_AcqRel:
		rs.PanicAt(skip+1, "there is no such thing as an acquire-release failure ordering")
	case Ordering_Release:
		rs.PanicAt(skip+1, "there is no such thing as a release failure ordering")
	default:
		panic(rs.Unreachable("rssync: invalid Ordering {}", int(failure)))
	}
}

// checkAny accepts every ordering, as swap and the fetch operations do.
func checkAny(order Ordering) {
	if order > Ordering_SeqCst {
		panic(rs.Unreachable("rssync: invalid Ordering {}", int(order)))
	}
}

// AtomicBool is std::sync::atomic::AtomicBool. The zero value is
// `AtomicBool::new(false)`, so a struct holding one needs no constructor.
//
// Share an atomic by pointer, as Rust shares it by reference: copying the Go
// value makes a second, independent atomic.
type AtomicBool struct {
	v uint32
}

func boolBits(b bool) uint32 {
	if b {
		return 1
	}
	return 0
}

// AtomicBoolNew is AtomicBool::new.
func AtomicBoolNew(v bool) AtomicBool { return AtomicBool{v: boolBits(v)} }

// AtomicBoolFrom is `impl From<bool> for AtomicBool`.
func AtomicBoolFrom(v bool) AtomicBool { return AtomicBoolNew(v) }

// AtomicBoolDefault is `impl Default for AtomicBool`: false.
func AtomicBoolDefault() AtomicBool { return AtomicBoolNew(false) }

// IntoInner is AtomicBool::into_inner.
func (a *AtomicBool) IntoInner() bool { return atomic.LoadUint32(&a.v) != 0 }

// Load is AtomicBool::load.
func (a *AtomicBool) Load(order Ordering) bool {
	checkLoad(order, 1)
	return atomic.LoadUint32(&a.v) != 0
}

// Store is AtomicBool::store.
func (a *AtomicBool) Store(val bool, order Ordering) {
	checkStore(order, 1)
	atomic.StoreUint32(&a.v, boolBits(val))
}

// Swap is AtomicBool::swap: it stores val and returns the previous value.
func (a *AtomicBool) Swap(val bool, order Ordering) bool {
	checkAny(order)
	return atomic.SwapUint32(&a.v, boolBits(val)) != 0
}

// CompareExchange is AtomicBool::compare_exchange: it stores new if the
// value is current. The result is the previous value, and whether it was
// current (Rust's Ok) or not (Err).
func (a *AtomicBool) CompareExchange(current, new bool, success, failure Ordering) (bool, bool) {
	checkCompareExchange(success, failure, 1)
	return a.compareExchange(current, new)
}

// CompareExchangeWeak is AtomicBool::compare_exchange_weak. The port never
// fails spuriously, which Rust allows but does not promise.
func (a *AtomicBool) CompareExchangeWeak(current, new bool, success, failure Ordering) (bool, bool) {
	checkCompareExchange(success, failure, 1)
	return a.compareExchange(current, new)
}

func (a *AtomicBool) compareExchange(current, new bool) (bool, bool) {
	for {
		old := atomic.LoadUint32(&a.v)
		if (old != 0) != current {
			return old != 0, false
		}
		if atomic.CompareAndSwapUint32(&a.v, old, boolBits(new)) {
			return old != 0, true
		}
	}
}

// fetch applies op to the value and returns the previous value.
func (a *AtomicBool) fetch(op func(bool) bool) bool {
	for {
		old := atomic.LoadUint32(&a.v)
		if atomic.CompareAndSwapUint32(&a.v, old, boolBits(op(old != 0))) {
			return old != 0
		}
	}
}

// FetchAnd is AtomicBool::fetch_and: logical "and" with val, returning the
// previous value.
func (a *AtomicBool) FetchAnd(val bool, order Ordering) bool {
	checkAny(order)
	return a.fetch(func(old bool) bool { return old && val })
}

// FetchNand is AtomicBool::fetch_nand.
func (a *AtomicBool) FetchNand(val bool, order Ordering) bool {
	checkAny(order)
	return a.fetch(func(old bool) bool { return !(old && val) })
}

// FetchOr is AtomicBool::fetch_or.
func (a *AtomicBool) FetchOr(val bool, order Ordering) bool {
	checkAny(order)
	return a.fetch(func(old bool) bool { return old || val })
}

// FetchXor is AtomicBool::fetch_xor.
func (a *AtomicBool) FetchXor(val bool, order Ordering) bool {
	checkAny(order)
	return a.fetch(func(old bool) bool { return old != val })
}

// FetchNot is AtomicBool::fetch_not.
func (a *AtomicBool) FetchNot(order Ordering) bool {
	checkAny(order)
	return a.fetch(func(old bool) bool { return !old })
}

// TryUpdate is AtomicBool::try_update: it applies f to the value until it
// stores f's result or f returns None. The result is the previous value, and
// whether f's result was stored (Rust's Ok) or f returned None (Err).
func (a *AtomicBool) TryUpdate(setOrder, fetchOrder Ordering, f func(bool) rs.Option[bool]) (bool, bool) {
	prev := a.Load(fetchOrder)
	for {
		next, ok := f(prev).Get()
		if !ok {
			return prev, false
		}
		if x, ok := a.CompareExchangeWeak(prev, next, setOrder, fetchOrder); ok {
			return x, true
		} else {
			prev = x
		}
	}
}

// FetchUpdate is AtomicBool::fetch_update, the deprecated name of
// TryUpdate.
func (a *AtomicBool) FetchUpdate(setOrder, fetchOrder Ordering, f func(bool) rs.Option[bool]) (bool, bool) {
	return a.TryUpdate(setOrder, fetchOrder, f)
}

// Update is AtomicBool::update: it stores f's result, retrying until no
// other thread changed the value in between, and returns the previous
// value.
func (a *AtomicBool) Update(setOrder, fetchOrder Ordering, f func(bool) bool) bool {
	prev := a.Load(fetchOrder)
	for {
		if x, ok := a.CompareExchangeWeak(prev, f(prev), setOrder, fetchOrder); ok {
			return x
		} else {
			prev = x
		}
	}
}

// FmtDebug is the Debug impl: the value's, read with Relaxed.
func (a *AtomicBool) FmtDebug(f *rs.Formatter) error {
	return rs.DebugFmt(a.Load(Ordering_Relaxed), f)
}

// atomicInteger is the integer types the atomic integers hold.
type atomicInteger interface {
	~int | ~int8 | ~int16 | ~int32 | ~int64 | ~uint | ~uint8 | ~uint16 | ~uint32 | ~uint64
}

// atomicInt is the body of every atomic integer. The value is kept as the
// 64-bit pattern uint64(T(x)), so that a load is a conversion back to T and
// every change is a compare-and-swap of the whole word; the arithmetic wraps
// at T's width, as Rust's atomic arithmetic does.
type atomicInt[T atomicInteger] struct {
	v uint64
}

// IntoInner is into_inner: the value.
func (a *atomicInt[T]) IntoInner() T { return T(atomic.LoadUint64(&a.v)) }

// Load is load.
func (a *atomicInt[T]) Load(order Ordering) T {
	checkLoad(order, 1)
	return T(atomic.LoadUint64(&a.v))
}

// Store is store.
func (a *atomicInt[T]) Store(val T, order Ordering) {
	checkStore(order, 1)
	atomic.StoreUint64(&a.v, uint64(val))
}

// Swap is swap: it stores val and returns the previous value.
func (a *atomicInt[T]) Swap(val T, order Ordering) T {
	checkAny(order)
	return T(atomic.SwapUint64(&a.v, uint64(val)))
}

// CompareExchange is compare_exchange: it stores new if the value is
// current. The result is the previous value, and whether it was current
// (Rust's Ok) or not (Err).
func (a *atomicInt[T]) CompareExchange(current, new T, success, failure Ordering) (T, bool) {
	checkCompareExchange(success, failure, 1)
	return a.compareExchange(current, new)
}

// CompareExchangeWeak is compare_exchange_weak. The port never fails
// spuriously, which Rust allows but does not promise.
func (a *atomicInt[T]) CompareExchangeWeak(current, new T, success, failure Ordering) (T, bool) {
	checkCompareExchange(success, failure, 1)
	return a.compareExchange(current, new)
}

func (a *atomicInt[T]) compareExchange(current, new T) (T, bool) {
	for {
		old := atomic.LoadUint64(&a.v)
		if T(old) != current {
			return T(old), false
		}
		if atomic.CompareAndSwapUint64(&a.v, old, uint64(new)) {
			return T(old), true
		}
	}
}

// fetch applies op to the value and returns the previous value.
func (a *atomicInt[T]) fetch(op func(T) T) T {
	for {
		old := atomic.LoadUint64(&a.v)
		if atomic.CompareAndSwapUint64(&a.v, old, uint64(op(T(old)))) {
			return T(old)
		}
	}
}

// FetchAdd is fetch_add: it adds val, wrapping around on overflow, and
// returns the previous value.
func (a *atomicInt[T]) FetchAdd(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return old + val })
}

// FetchSub is fetch_sub: it subtracts val, wrapping around on overflow, and
// returns the previous value.
func (a *atomicInt[T]) FetchSub(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return old - val })
}

// FetchAnd is fetch_and: bitwise "and" with val, returning the previous
// value.
func (a *atomicInt[T]) FetchAnd(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return old & val })
}

// FetchNand is fetch_nand.
func (a *atomicInt[T]) FetchNand(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return ^(old & val) })
}

// FetchOr is fetch_or.
func (a *atomicInt[T]) FetchOr(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return old | val })
}

// FetchXor is fetch_xor.
func (a *atomicInt[T]) FetchXor(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return old ^ val })
}

// FetchMax is fetch_max: it stores the larger of the value and val, and
// returns the previous value.
func (a *atomicInt[T]) FetchMax(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return max(old, val) })
}

// FetchMin is fetch_min: it stores the smaller of the value and val, and
// returns the previous value.
func (a *atomicInt[T]) FetchMin(val T, order Ordering) T {
	checkAny(order)
	return a.fetch(func(old T) T { return min(old, val) })
}

// TryUpdate is try_update: it applies f to the value until it stores f's
// result or f returns None. The result is the previous value, and whether
// f's result was stored (Rust's Ok) or f returned None (Err).
func (a *atomicInt[T]) TryUpdate(setOrder, fetchOrder Ordering, f func(T) rs.Option[T]) (T, bool) {
	prev := a.Load(fetchOrder)
	for {
		next, ok := f(prev).Get()
		if !ok {
			return prev, false
		}
		if x, ok := a.CompareExchangeWeak(prev, next, setOrder, fetchOrder); ok {
			return x, true
		} else {
			prev = x
		}
	}
}

// FetchUpdate is fetch_update, the deprecated name of TryUpdate.
func (a *atomicInt[T]) FetchUpdate(setOrder, fetchOrder Ordering, f func(T) rs.Option[T]) (T, bool) {
	return a.TryUpdate(setOrder, fetchOrder, f)
}

// Update is update: it stores f's result, retrying until no other thread
// changed the value in between, and returns the previous value.
func (a *atomicInt[T]) Update(setOrder, fetchOrder Ordering, f func(T) T) T {
	prev := a.Load(fetchOrder)
	for {
		if x, ok := a.CompareExchangeWeak(prev, f(prev), setOrder, fetchOrder); ok {
			return x
		} else {
			prev = x
		}
	}
}

// FmtDebug is the Debug impl: the value's, read with Relaxed.
func (a *atomicInt[T]) FmtDebug(f *rs.Formatter) error {
	return rs.DebugFmt(a.Load(Ordering_Relaxed), f)
}

// AtomicU8 is std::sync::atomic::AtomicU8. The zero value holds 0. Its
// methods are atomicInt's: Load, Store, Swap, CompareExchange,
// CompareExchangeWeak, FetchAdd, FetchSub, FetchAnd, FetchNand, FetchOr,
// FetchXor, FetchMax, FetchMin, TryUpdate, FetchUpdate, Update, IntoInner.
type AtomicU8 struct{ atomicInt[uint8] }

// AtomicU8New is AtomicU8::new.
func AtomicU8New(v uint8) AtomicU8 { return AtomicU8{atomicInt[uint8]{v: uint64(v)}} }

// AtomicU8From is `impl From<u8> for AtomicU8`.
func AtomicU8From(v uint8) AtomicU8 { return AtomicU8New(v) }

// AtomicU8Default is `impl Default for AtomicU8`: 0.
func AtomicU8Default() AtomicU8 { return AtomicU8New(0) }

// AtomicU64 is std::sync::atomic::AtomicU64. The zero value holds 0. Its
// methods are AtomicU8's.
type AtomicU64 struct{ atomicInt[uint64] }

// AtomicU64New is AtomicU64::new.
func AtomicU64New(v uint64) AtomicU64 { return AtomicU64{atomicInt[uint64]{v: v}} }

// AtomicU64From is `impl From<u64> for AtomicU64`.
func AtomicU64From(v uint64) AtomicU64 { return AtomicU64New(v) }

// AtomicU64Default is `impl Default for AtomicU64`: 0.
func AtomicU64Default() AtomicU64 { return AtomicU64New(0) }

// AtomicUsize is std::sync::atomic::AtomicUsize, holding the port's usize,
// int. The zero value holds 0. Its methods are AtomicU8's; the arithmetic
// wraps at int's range (translation-rules.md, rule 3: usize is int).
type AtomicUsize struct{ atomicInt[int] }

// AtomicUsizeNew is AtomicUsize::new.
func AtomicUsizeNew(v int) AtomicUsize { return AtomicUsize{atomicInt[int]{v: uint64(v)}} }

// AtomicUsizeFrom is `impl From<usize> for AtomicUsize`.
func AtomicUsizeFrom(v int) AtomicUsize { return AtomicUsizeNew(v) }

// AtomicUsizeDefault is `impl Default for AtomicUsize`: 0.
func AtomicUsizeDefault() AtomicUsize { return AtomicUsizeNew(0) }

// AtomicI64 is std::sync::atomic::AtomicI64. The zero value holds 0. Its
// methods are AtomicU8's.
type AtomicI64 struct{ atomicInt[int64] }

// AtomicI64New is AtomicI64::new.
func AtomicI64New(v int64) AtomicI64 { return AtomicI64{atomicInt[int64]{v: uint64(v)}} }

// AtomicI64From is `impl From<i64> for AtomicI64`.
func AtomicI64From(v int64) AtomicI64 { return AtomicI64New(v) }

// AtomicI64Default is `impl Default for AtomicI64`: 0.
func AtomicI64Default() AtomicI64 { return AtomicI64New(0) }
