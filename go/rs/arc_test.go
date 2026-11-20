package rs

import (
	"math"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// probeCanary counts how often it is dropped, as Canary does in the Rust
// tests.
type probeCanary struct {
	drops *atomic.Int64
	label string
}

func (c *probeCanary) Drop() { c.drops.Add(1) }

func newCanary() (probeCanary, *atomic.Int64) {
	drops := &atomic.Int64{}
	return probeCanary{drops: drops, label: "canary"}, drops
}

// waitFor runs the garbage collector until done reports true: cleanups run
// some time after a collection, on other goroutines.
func waitFor(t *testing.T, what string, done func() bool) {
	t.Helper()
	deadline := time.Now().Add(20 * time.Second)
	for !done() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		runtime.GC()
		time.Sleep(time.Millisecond)
	}
}

// formatterReady reports whether the formatter has been ported; until then
// the tests that print skip.
func formatterReady() (ready bool) {
	defer func() {
		if recover() != nil {
			ready = false
		}
	}()
	return Format("{:>3}|{:?}", 1, "a") == `  1|"a"`
}

func TestArc_uninhabited(t *testing.T) {
	// Port of alloctests arc.rs uninhabited and trait_object, as far as a
	// dangling Weak goes.
	weak := WeakNew[struct{}]()
	weak = weak.Clone()
	if weak.Upgrade().IsSome() {
		t.Fatal("Weak::new() never upgrades")
	}
	weak.Drop()
	weak.Drop()
}

func TestArc_float_nan_ne(t *testing.T) {
	// Port of alloctests arc.rs float_nan_ne.
	x := ArcNew(float32(math.NaN()))
	defer x.Drop()
	if Eq(x, x) {
		t.Fatal("x != x")
	}
	whole := ArcNew(float32(1))
	defer whole.Drop()
	if !Eq(whole, whole) {
		t.Fatal("an Arc of a number is equal to itself")
	}
}

// probeCountedEq is TestPEq of alloctests arc.rs: its eq counts its calls on
// both operands.
type probeCountedEq struct{ calls Cell[int] }

func (p *probeCountedEq) Equal(other *probeCountedEq) bool {
	p.calls.Set(p.calls.Get() + 1)
	other.calls.Set(other.calls.Get() + 1)
	return true
}

func TestArc_partial_eq(t *testing.T) {
	// Port of alloctests arc.rs partial_eq: for a T that is only PartialEq,
	// an Arc compared with itself still asks T.
	x := ArcNew(probeCountedEq{})
	defer x.Drop()
	if !Eq(x, x) {
		t.Fatal("x == x")
	}
	if !Eq(x, x) {
		t.Fatal("!(x != x)")
	}
	if got := x.Get().calls.Get(); got != 4 {
		t.Fatalf("eq ran on %d operands, want 4", got)
	}
}

func TestArc_eq(t *testing.T) {
	// Alloctests arc.rs eq gives the same type `#[derive(Eq)]` and expects no
	// call at all, since Rust then trusts pointer equality. Go has no Eq
	// marker to see: a hand-written Equal is always asked (eq.go), so the
	// result is Rust's and the number of calls is not.
	x := ArcNew(probeCountedEq{})
	defer x.Drop()
	if !Eq(x, x) {
		t.Fatal("x == x")
	}
	// Where the shape of T proves equality reflexive, nothing is compared:
	// a value this deep would take visibly long otherwise.
	deep := ArcNew(make([][]int, 1))
	defer deep.Drop()
	if !Eq(deep, deep) {
		t.Fatal("an Arc is equal to itself")
	}
}

type probePanicOnDrop struct{}

func (probePanicOnDrop) Drop() { panic("PanicOnDrop") }

func TestArc_panic_no_leak(t *testing.T) {
	// Port of alloctests arc.rs panic_no_leak: a panic from the destructor
	// reaches whoever dropped the last handle, and the allocation's own weak
	// count is released all the same.
	arc := ArcNew(probePanicOnDrop{})
	weak := ArcDowngrade(arc)
	defer weak.Drop()
	inner := arc.handle.inner
	if inner.weak.Load() != 2 {
		t.Fatalf("weak = %d before the drop", inner.weak.Load())
	}
	expectPanic(t, "PanicOnDrop", func() { arc.Drop() })
	if inner.strong.Load() != 0 || inner.weak.Load() != 1 {
		t.Fatalf("after a panicking destructor: strong %d, weak %d", inner.strong.Load(), inner.weak.Load())
	}
	if weak.Upgrade().IsSome() {
		t.Fatal("the value is gone")
	}
}

func TestArc_make_mut_unsized(t *testing.T) {
	// Port of alloctests arc.rs make_mut_unsized.
	data := ArcNew([]int32{10, 20, 30})
	defer data.Drop()

	(*ArcMakeMut(&data))[0] += 1 // Won't clone anything
	otherData := data.Clone()    // Won't clone inner data
	defer otherData.Drop()
	(*ArcMakeMut(&data))[1] += 1       // Clones inner data
	(*ArcMakeMut(&data))[2] += 1       // Won't clone anything
	(*ArcMakeMut(&otherData))[0] *= 10 // Won't clone anything

	// Now `data` and `other_data` point to different allocations.
	if !Eq(*data.Get(), []int32{11, 21, 31}) || !Eq(*otherData.Get(), []int32{110, 20, 30}) {
		t.Fatalf("data = %v, other_data = %v", *data.Get(), *otherData.Get())
	}
}

func TestArc_manually_share_arc(t *testing.T) {
	// Port of alloctests sync.rs manually_share_arc.
	arcV := ArcNew([]int{1, 2, 3, 4, 5, 6, 7, 8, 9, 10})
	defer arcV.Drop()
	channel := make(chan Arc[[]int])
	done := make(chan int)
	go func() {
		received := <-channel
		defer received.Drop()
		done <- (*received.Get())[3]
	}()
	channel <- arcV.Clone()
	if got := <-done; got != 4 {
		t.Fatalf("the other thread read %d", got)
	}
	if (*arcV.Get())[2] != 3 || (*arcV.Get())[4] != 5 {
		t.Fatal("the value changed")
	}
}

func TestArc_test_arc_get_mut(t *testing.T) {
	// Port of alloctests sync.rs test_arc_get_mut.
	x := ArcNew(3)
	defer x.Drop()
	*ArcGetMut(x).Unwrap() = 4
	if *x.Get() != 4 {
		t.Fatal("*x == 4")
	}
	y := x.Clone()
	if ArcGetMut(x).IsSome() {
		t.Fatal("get_mut with a second Arc")
	}
	y.Drop()
	if ArcGetMut(x).IsNone() {
		t.Fatal("get_mut after the second Arc was dropped")
	}
	w := ArcDowngrade(x)
	defer w.Drop()
	if ArcGetMut(x).IsSome() {
		t.Fatal("get_mut with a Weak")
	}
}

func TestArc_weak_counts(t *testing.T) {
	// Port of alloctests sync.rs weak_counts.
	if empty := WeakNew[uint64](); empty.WeakCount() != 0 || empty.StrongCount() != 0 {
		t.Fatal("the counts of Weak::new()")
	}

	a := ArcNew(0)
	w := ArcDowngrade(a)
	if w.StrongCount() != 1 || w.WeakCount() != 1 {
		t.Fatal("one Arc, one Weak")
	}
	w2 := w.Clone()
	if w.StrongCount() != 1 || w.WeakCount() != 2 || w2.StrongCount() != 1 || w2.WeakCount() != 2 {
		t.Fatal("one Arc, two Weaks")
	}
	w.Drop()
	if w2.StrongCount() != 1 || w2.WeakCount() != 1 {
		t.Fatal("after dropping one Weak")
	}
	a2 := a.Clone()
	if w2.StrongCount() != 2 || w2.WeakCount() != 1 {
		t.Fatal("two Arcs, one Weak")
	}
	a2.Drop()
	a.Drop()
	if w2.StrongCount() != 0 || w2.WeakCount() != 0 {
		t.Fatal("no Arc left")
	}
	w2.Drop()
}

func TestArc_try_unwrap(t *testing.T) {
	// Port of alloctests sync.rs try_unwrap.
	x := ArcNew(3)
	if value, ok := ArcTryUnwrap(x); !ok || value != 3 {
		t.Fatal("Arc::try_unwrap(x) == Ok(3)")
	}
	x = ArcNew(4)
	y := x.Clone()
	defer y.Drop()
	if _, ok := ArcTryUnwrap(x); ok {
		t.Fatal("try_unwrap with a second Arc")
	}
	// Err hands the Arc back: it is still usable and still counts.
	if *x.Get() != 4 || ArcStrongCount(x) != 2 {
		t.Fatal("a refused try_unwrap must leave the handle alone")
	}
	x.Drop()
	x = ArcNew(5)
	w := ArcDowngrade(x)
	defer w.Drop()
	if value, ok := ArcTryUnwrap(x); !ok || value != 5 {
		t.Fatal("try_unwrap succeeds with only Weaks around")
	}
	if w.Upgrade().IsSome() || w.StrongCount() != 0 {
		t.Fatal("the Weak must not upgrade after try_unwrap")
	}
}

func TestArc_into_inner(t *testing.T) {
	// Port of alloctests sync.rs into_inner.
	for range 100 {
		x := ArcNew(3)
		y := x.Clone()
		var r, s Option[int]
		var wait sync.WaitGroup
		wait.Add(2)
		go func() { defer wait.Done(); r = ArcIntoInner(x) }()
		go func() { defer wait.Done(); s = ArcIntoInner(y) }()
		wait.Wait()
		if !(r.IsNone() && s == Some(3)) && !(r == Some(3) && s.IsNone()) {
			t.Fatalf("unexpected result (%+v, %+v), expected (None, Some(3)) or (Some(3), None)", r, s)
		}
	}

	x := ArcNew(3)
	if ArcIntoInner(x) != Some(3) {
		t.Fatal("into_inner of the only Arc")
	}

	x = ArcNew(4)
	y := x.Clone()
	if ArcIntoInner(x).IsSome() {
		t.Fatal("into_inner with a second Arc")
	}
	if ArcIntoInner(y) != Some(4) {
		t.Fatal("into_inner of the last Arc")
	}

	x = ArcNew(5)
	w := ArcDowngrade(x)
	defer w.Drop()
	if ArcIntoInner(x) != Some(5) {
		t.Fatal("into_inner with only a Weak around")
	}
}

func TestArc_test_cowarc_clone_make_mut(t *testing.T) {
	// Port of alloctests sync.rs test_cowarc_clone_make_mut.
	cow0 := ArcNew(75)
	cow1 := cow0.Clone()
	cow2 := cow1.Clone()
	defer cow0.Drop()
	defer cow1.Drop()
	defer cow2.Drop()

	if *ArcMakeMut(&cow0) != 75 || *ArcMakeMut(&cow1) != 75 || *ArcMakeMut(&cow2) != 75 {
		t.Fatal("make_mut keeps the value")
	}

	*ArcMakeMut(&cow0) += 1
	*ArcMakeMut(&cow1) += 2
	*ArcMakeMut(&cow2) += 3

	if *cow0.Get() != 76 || *cow1.Get() != 77 || *cow2.Get() != 78 {
		t.Fatalf("%d %d %d", *cow0.Get(), *cow1.Get(), *cow2.Get())
	}
	// none should point to the same backing memory
	if ArcPtrEq(cow0, cow1) || ArcPtrEq(cow0, cow2) || ArcPtrEq(cow1, cow2) {
		t.Fatal("the three must not share a value")
	}
	if ArcStrongCount(cow0) != 1 || ArcStrongCount(cow1) != 1 || ArcStrongCount(cow2) != 1 {
		t.Fatal("each is the only handle to its value")
	}
}

func TestArc_test_cowarc_clone_unique2(t *testing.T) {
	// Port of alloctests sync.rs test_cowarc_clone_unique2.
	cow0 := ArcNew(75)
	cow1 := cow0.Clone()
	cow2 := cow1.Clone()
	defer cow0.Drop()
	defer cow1.Drop()
	defer cow2.Drop()

	*ArcMakeMut(&cow0) += 1
	if *cow0.Get() != 76 || *cow1.Get() != 75 || *cow2.Get() != 75 {
		t.Fatalf("%d %d %d", *cow0.Get(), *cow1.Get(), *cow2.Get())
	}
	// cow1 and cow2 should share the same contents
	// cow0 should have a unique reference
	if ArcPtrEq(cow0, cow1) || ArcPtrEq(cow0, cow2) || !ArcPtrEq(cow1, cow2) {
		t.Fatal("cow1 and cow2 share, cow0 is alone")
	}
	if ArcStrongCount(cow0) != 1 || ArcStrongCount(cow1) != 2 {
		t.Fatalf("counts %d and %d", ArcStrongCount(cow0), ArcStrongCount(cow1))
	}
}

func TestArc_test_cowarc_clone_weak(t *testing.T) {
	// Port of alloctests sync.rs test_cowarc_clone_weak.
	cow0 := ArcNew(75)
	defer cow0.Drop()
	cow1Weak := ArcDowngrade(cow0)
	defer cow1Weak.Drop()

	upgraded := cow1Weak.Upgrade().Unwrap()
	if *cow0.Get() != 75 || *upgraded.Get() != 75 {
		t.Fatal("both see 75")
	}
	upgraded.Drop()

	*ArcMakeMut(&cow0) += 1

	if *cow0.Get() != 76 {
		t.Fatal("76 == *cow0")
	}
	if cow1Weak.Upgrade().IsSome() {
		t.Fatal("make_mut must cut the Weak off")
	}
	if ArcStrongCount(cow0) != 1 || ArcWeakCount(cow0) != 0 {
		t.Fatal("the new allocation has one handle")
	}
}

func TestArc_test_live_and_test_dead(t *testing.T) {
	// Port of alloctests sync.rs test_live and test_dead.
	x := ArcNew(5)
	y := ArcDowngrade(x)
	defer y.Drop()
	upgraded := y.Upgrade()
	if upgraded.IsNone() {
		t.Fatal("a Weak upgrades while an Arc lives")
	}
	upgraded.AsRef().Drop()
	x.Drop()
	if y.Upgrade().IsSome() {
		t.Fatal("a Weak does not upgrade once the Arcs are gone")
	}
}

type probeCycle struct {
	x Option[Weak[probeCycle]]
}

func TestArc_weak_self_cyclic(t *testing.T) {
	// Port of alloctests sync.rs weak_self_cyclic.
	a := ArcNew(probeCycle{})
	temporary := a.Clone()
	b := ArcDowngrade(temporary)
	temporary.Drop()
	a.Get().x = Some(b)
	if ArcStrongCount(a) != 1 || ArcWeakCount(a) != 1 {
		t.Fatal("one Arc and the Weak it holds to itself")
	}
	// hopefully we don't double-free (or leak)...
	a.Drop()
	b.Drop()
}

func TestArc_drop_arc(t *testing.T) {
	// Port of alloctests sync.rs drop_arc and drop_arc_weak.
	canary, drops := newCanary()
	x := ArcNew(canary)
	x.Drop()
	if drops.Load() != 1 {
		t.Fatal("the value is dropped with its only Arc")
	}

	canary, drops = newCanary()
	arc := ArcNew(canary)
	arcWeak := ArcDowngrade(arc)
	if drops.Load() != 0 {
		t.Fatal("nothing dropped yet")
	}
	arc.Drop()
	if drops.Load() != 1 {
		t.Fatal("the value is dropped with the last Arc, whatever Weaks remain")
	}
	arcWeak.Drop()
	if drops.Load() != 1 {
		t.Fatal("dropping the Weak drops nothing more")
	}
}

func TestArc_test_strong_count(t *testing.T) {
	// Port of alloctests sync.rs test_strong_count.
	a := ArcNew(0)
	if ArcStrongCount(a) != 1 {
		t.Fatal("1")
	}
	w := ArcDowngrade(a)
	if ArcStrongCount(a) != 1 {
		t.Fatal("a Weak is not a strong handle")
	}
	b := w.Upgrade().Expect("")
	if ArcStrongCount(b) != 2 || ArcStrongCount(a) != 2 {
		t.Fatal("2 after the upgrade")
	}
	w.Drop()
	a.Drop()
	if ArcStrongCount(b) != 1 {
		t.Fatal("1 after dropping a")
	}
	c := b.Clone()
	if ArcStrongCount(b) != 2 || ArcStrongCount(c) != 2 {
		t.Fatal("2 after the clone")
	}
	b.Drop()
	c.Drop()
}

func TestArc_test_weak_count(t *testing.T) {
	// Port of alloctests sync.rs test_weak_count.
	a := ArcNew(0)
	if ArcStrongCount(a) != 1 || ArcWeakCount(a) != 0 {
		t.Fatal("a fresh Arc")
	}
	w := ArcDowngrade(a)
	if ArcStrongCount(a) != 1 || ArcWeakCount(a) != 1 {
		t.Fatal("one Weak")
	}
	x := w.Clone()
	if ArcWeakCount(a) != 2 {
		t.Fatal("two Weaks")
	}
	w.Drop()
	x.Drop()
	if ArcStrongCount(a) != 1 || ArcWeakCount(a) != 0 {
		t.Fatal("no Weak left")
	}
	c := a.Clone()
	if ArcStrongCount(a) != 2 || ArcWeakCount(a) != 0 {
		t.Fatal("two Arcs")
	}
	d := ArcDowngrade(c)
	if ArcWeakCount(c) != 1 || ArcStrongCount(c) != 2 {
		t.Fatal("two Arcs, one Weak")
	}

	a.Drop()
	c.Drop()
	d.Drop()
}

func TestArc_show_arc(t *testing.T) {
	// Port of alloctests sync.rs show_arc.
	if !formatterReady() {
		t.Skip("the formatter is not ported yet")
	}
	a := ArcNew(5)
	defer a.Drop()
	if got := Format("{a:?}", Arg("a", a)); got != "5" {
		t.Fatalf("{a:?} = %q", got)
	}
	weak := ArcDowngrade(a)
	defer weak.Drop()
	if got := DebugString(weak); got != "(Weak)" {
		t.Fatalf("a Weak prints as %q", got)
	}
}

// probeFoo is _Foo of alloctests sync.rs: deriving works with Arc<T>.
type probeFoo struct {
	inner Arc[int32] `rs:"inner"`
}

func TestArcInADerivingStruct(t *testing.T) {
	foo := probeFoo{inner: ArcNew(int32(7))}
	defer foo.inner.Drop()

	cloned := Clone(foo)
	if ArcStrongCount(foo.inner) != 2 || !ArcPtrEq(foo.inner, cloned.inner) {
		t.Fatal("the derived Clone of a struct holding an Arc is another handle to the same value")
	}
	if cloned.inner.handle == foo.inner.handle {
		t.Fatal("the clone must be a handle of its own")
	}
	if !Eq(foo, cloned) || Cmp(foo, cloned) != 0 {
		t.Fatal("a struct is equal to its clone")
	}
	cloned.inner.Drop()
	if ArcStrongCount(foo.inner) != 1 || *foo.inner.Get() != 7 {
		t.Fatal("dropping the clone's handle leaves the original")
	}

	other := probeFoo{inner: ArcNew(int32(9))}
	defer other.inner.Drop()
	if Eq(foo, other) || !(Cmp(foo, other) < 0) || !(Cmp(other, foo) > 0) {
		t.Fatal("Arcs compare by the values they hold")
	}
	same := probeFoo{inner: ArcNew(int32(7))}
	defer same.inner.Drop()
	if !Eq(foo, same) {
		t.Fatal("two Arcs of equal values are equal")
	}

	defaulted := Default[probeFoo]()
	defer defaulted.inner.Drop()
	if *defaulted.inner.Get() != 0 || ArcStrongCount(defaulted.inner) != 1 {
		t.Fatal("the Default of an Arc is Arc::new(T::default())")
	}

	// Collections of Arcs clone handle by handle.
	list := []Arc[int32]{foo.inner.Clone(), other.inner.Clone()}
	listClone := Clone(list)
	if ArcStrongCount(foo.inner) != 3 || ArcStrongCount(other.inner) != 3 {
		t.Fatal("cloning a Vec<Arc<T>> clones each Arc")
	}
	for i := range list {
		list[i].Drop()
		listClone[i].Drop()
	}
	maybe := Some(foo.inner.Clone())
	maybeClone := Clone(maybe)
	if ArcStrongCount(foo.inner) != 3 {
		t.Fatal("cloning an Option<Arc<T>> clones the Arc")
	}
	maybe.AsRef().Drop()
	maybeClone.AsRef().Drop()
	if ArcStrongCount(foo.inner) != 1 || ArcStrongCount(other.inner) != 1 {
		t.Fatal("every clone was dropped")
	}

	// A Weak in a struct clones to another Weak.
	type observer struct{ target Weak[int32] }
	watching := observer{target: ArcDowngrade(foo.inner)}
	watchingClone := Clone(watching)
	if ArcWeakCount(foo.inner) != 2 {
		t.Fatal("cloning a struct holding a Weak clones the Weak")
	}
	watching.target.Drop()
	watchingClone.target.Drop()
	if ArcWeakCount(foo.inner) != 0 {
		t.Fatal("both Weaks were dropped")
	}
}

func TestArc_test_new_weak_and_test_ptr_eq(t *testing.T) {
	// Port of alloctests sync.rs test_new_weak and test_ptr_eq, and of the
	// examples of Weak::ptr_eq.
	foo := WeakNew[int]()
	if foo.Upgrade().IsSome() {
		t.Fatal("Weak::new() does not upgrade")
	}

	five := ArcNew(5)
	sameFive := five.Clone()
	otherFive := ArcNew(5)
	defer five.Drop()
	defer sameFive.Drop()
	defer otherFive.Drop()
	if !ArcPtrEq(five, sameFive) || ArcPtrEq(five, otherFive) {
		t.Fatal("Arc::ptr_eq")
	}
	if ArcAsPtr(five) != ArcAsPtr(sameFive) || ArcAsPtr(five) == ArcAsPtr(otherFive) || ArcAsPtr(five) != five.Get() {
		t.Fatal("Arc::as_ptr")
	}

	first := ArcDowngrade(five)
	second := ArcDowngrade(five)
	third := ArcDowngrade(otherFive)
	defer first.Drop()
	defer second.Drop()
	defer third.Drop()
	if !first.PtrEq(second) || first.PtrEq(third) || !WeakPtrEq(first, second) {
		t.Fatal("Weak::ptr_eq")
	}
	if !WeakNew[int]().PtrEq(WeakNew[int]()) || WeakNew[int]().PtrEq(third) {
		t.Fatal("two Weak::new() are equal to each other and to nothing else")
	}
}

func TestArc_test_weak_count_locked(t *testing.T) {
	// Port of alloctests sync.rs test_weak_count_locked: get_mut locks the
	// weak count, and nobody may read the lock as a count.
	a := ArcNew(atomic.Bool{})
	a2 := a.Clone()
	defer a.Drop()
	defer a2.Drop()
	done := make(chan struct{})
	go func() {
		defer close(done)
		for range 1000000 {
			ArcGetMut(a)
		}
		a.Get().Store(true)
	}()
	for !a2.Get().Load() {
		if n := ArcWeakCount(a2); n >= 2 {
			t.Errorf("bad weak count: %d", n)
			break
		}
	}
	<-done
}

func TestArc_arc_drop_dereferenceable_race(t *testing.T) {
	// Port of alloctests sync.rs arc_drop_dereferenceable_race.
	for range 750 {
		canary, drops := newCanary()
		arc1 := ArcNew(canary)
		arc2 := arc1.Clone()
		done := make(chan struct{})
		go func() { arc2.Drop(); close(done) }()
		arc1.Drop()
		<-done
		if drops.Load() != 1 {
			t.Fatalf("the value was dropped %d times", drops.Load())
		}
	}
}

func TestArc_test_from_owned_and_test_from_box(t *testing.T) {
	// Port of alloctests sync.rs test_from_owned and test_from_box.
	foo := 123
	fooArc := ArcNew(foo)
	defer fooArc.Drop()
	if *fooArc.Get() != 123 {
		t.Fatal("123 == *foo_arc")
	}
	box := new(uint32)
	*box = 123
	r := ArcFromBox(box)
	defer r.Drop()
	if *r.Get() != 123 || r.Get() != box {
		t.Fatal("Arc::from(box) takes the box over")
	}
	expectPanic(t, "the box is nil", func() { ArcFromBox[int](nil) })
}

func TestArcDropsTheValueOnce(t *testing.T) {
	canary, drops := newCanary()
	first := ArcNew(canary)
	second := first.Clone()
	third := second.Clone()
	first.Drop()
	first.Drop() // idempotent
	second.Drop()
	if drops.Load() != 0 {
		t.Fatal("dropped while a handle remains")
	}
	// A Go copy of an Arc is the same handle: dropping either drops both.
	moved := third
	moved.Drop()
	third.Drop()
	if drops.Load() != 1 {
		t.Fatalf("dropped %d times", drops.Load())
	}

	var zero Arc[probeCanary]
	zero.Drop() // nothing to do

	// A value moved out by try_unwrap or into_inner is not dropped.
	canary, drops = newCanary()
	if _, ok := ArcTryUnwrap(ArcNew(canary)); !ok || drops.Load() != 0 {
		t.Fatal("try_unwrap moves the value out without dropping it")
	}
	if ArcIntoInner(ArcNew(canary)).IsNone() || drops.Load() != 0 {
		t.Fatal("into_inner moves the value out without dropping it")
	}
	// into_inner of one of two handles gives the handle up.
	one := ArcNew(canary)
	two := one.Clone()
	if ArcIntoInner(one).IsSome() || ArcStrongCount(two) != 1 || drops.Load() != 0 {
		t.Fatal("into_inner of a shared Arc drops only the handle")
	}
	two.Drop()
	if drops.Load() != 1 {
		t.Fatal("the last handle drops the value")
	}

	// The destructor of a value held as an interface or as a pointer.
	canary, drops = newCanary()
	asInterface := ArcNew[Dropper](&canary)
	asInterface.Drop()
	asPointer := ArcNew(&canary)
	asPointer.Drop()
	if drops.Load() != 2 {
		t.Fatalf("destructors behind an interface and a pointer ran %d times", drops.Load())
	}
	nilPointer := ArcNew[*probeCanary](nil)
	nilPointer.Drop()
	plain := ArcNew("no destructor")
	plain.Drop()
}

func TestArcRejectsAHandleThatIsGone(t *testing.T) {
	dropped := ArcNew(1)
	dropped.Drop()
	expectPanic(t, "rs.Arc: Get on a handle that was already dropped", func() { dropped.Get() })
	expectPanic(t, "rs.Arc: Clone on a handle that was already dropped", func() { dropped.Clone() })
	expectPanic(t, "already dropped", func() { ArcStrongCount(dropped) })
	expectPanic(t, "already dropped", func() { ArcDowngrade(dropped) })
	expectPanic(t, "already dropped", func() { ArcMakeMut(&dropped) })

	taken := ArcNew(2)
	ArcTryUnwrap(taken)
	expectPanic(t, "already dropped", func() { taken.Get() })
	taken.Drop() // nothing left to drop

	var zero Arc[int]
	expectPanic(t, "rs.Arc: Get on the zero Arc", func() { zero.Get() })
	expectPanic(t, "the zero Arc", func() { zero.Clone() })

	live := ArcNew(3)
	defer live.Drop()
	weak := ArcDowngrade(live)
	weak.Drop()
	weak.Drop()
	expectPanic(t, "rs.Weak: Upgrade on a handle that was already dropped", func() { weak.Upgrade() })
	expectPanic(t, "already dropped", func() { weak.Clone() })
	if ArcWeakCount(live) != 0 {
		t.Fatal("a Weak dropped twice counts once")
	}

	// The panic names the caller, as #[track_caller] does.
	defer func() {
		info := PanicInfoOf(recover())
		if !strings.HasSuffix(info.File, "arc_test.go") {
			t.Fatalf("the panic is attributed to %s:%d", info.File, info.Line)
		}
	}()
	dropped.Get()
}

func TestArcUnwrapOrClone(t *testing.T) {
	// Port of the example of Arc::unwrap_or_clone.
	inner := []string{"test"}
	arc := ArcNew(inner)
	unwrapped := ArcUnwrapOrClone(arc)
	// The inner value was not cloned
	if &unwrapped[0] != &inner[0] {
		t.Fatal("the only Arc gives its value up")
	}

	arc = ArcNew(unwrapped)
	arc2 := arc.Clone()
	cloned := ArcUnwrapOrClone(arc)
	// Because there were 2 references, we had to clone the inner value.
	if &cloned[0] == &inner[0] || cloned[0] != "test" {
		t.Fatal("a shared Arc gives a clone")
	}
	if ArcStrongCount(arc2) != 1 {
		t.Fatal("unwrap_or_clone uses its Arc up")
	}
	// `arc2` is the last reference, so when we unwrap it we get back
	// the original `String`.
	last := ArcUnwrapOrClone(arc2)
	if &last[0] != &inner[0] {
		t.Fatal("the last Arc gives its value up")
	}
}

func TestArcMakeMut(t *testing.T) {
	// Port of the examples of Arc::make_mut.
	data := ArcNew(5)
	*ArcMakeMut(&data) += 1      // Won't clone anything
	otherData := data.Clone()    // Won't clone inner data
	*ArcMakeMut(&data) += 1      // Clones inner data
	*ArcMakeMut(&data) += 1      // Won't clone anything
	*ArcMakeMut(&otherData) *= 2 // Won't clone anything
	// Now `data` and `other_data` point to different allocations.
	if *data.Get() != 8 || *otherData.Get() != 12 {
		t.Fatalf("%d %d", *data.Get(), *otherData.Get())
	}
	data.Drop()
	otherData.Drop()

	// The clone make_mut takes is a deep one, and the value it leaves is
	// dropped by whoever holds it last, not by make_mut.
	canary, drops := newCanary()
	type payload struct {
		canary probeCanary
		items  []int
	}
	shared := ArcNew(payload{canary: canary, items: []int{1}})
	keeper := shared.Clone()
	before := shared.handle
	ArcMakeMut(&shared).items[0] = 2
	if shared.handle == before || before.gone.Load() != true {
		t.Fatal("make_mut on a shared Arc replaces the handle")
	}
	if keeper.Get().items[0] != 1 || shared.Get().items[0] != 2 || ArcStrongCount(keeper) != 1 || ArcStrongCount(shared) != 1 {
		t.Fatal("make_mut on a shared Arc leaves the other handle its value")
	}
	if drops.Load() != 0 {
		t.Fatal("nothing is dropped while each value has a handle")
	}
	shared.Drop()
	keeper.Drop()

	// With only Weaks around, the value moves: it is neither cloned nor
	// dropped, and the pointer stays good.
	moving := ArcNew(payload{items: []int{1}})
	weak := ArcDowngrade(moving)
	address := moving.Get()
	if ArcMakeMut(&moving) != address {
		t.Fatal("make_mut moves the value; it does not copy it")
	}
	if weak.Upgrade().IsSome() || weak.StrongCount() != 0 || ArcStrongCount(moving) != 1 || ArcWeakCount(moving) != 0 {
		t.Fatal("the Weak is cut off and the Arc is alone on a new allocation")
	}
	weak.Drop()
	moving.Drop()
}

func TestArcFromManyGoroutines(t *testing.T) {
	canary, drops := newCanary()
	root := ArcNew(canary)
	const workers, rounds = 16, 2000
	var wait sync.WaitGroup
	for worker := range workers {
		wait.Add(1)
		go func() {
			defer wait.Done()
			mine := root.Clone()
			defer mine.Drop()
			for round := range rounds {
				clone := mine.Clone()
				weak := ArcDowngrade(clone)
				upgraded := weak.Upgrade()
				if upgraded.IsNone() {
					t.Errorf("worker %d: a Weak failed to upgrade while its Arc lives", worker)
				}
				if clone.Get().label != "canary" {
					t.Errorf("worker %d: the value changed", worker)
				}
				// Unique it is not: at least mine and clone exist.
				if ArcGetMut(clone).IsSome() {
					t.Errorf("worker %d: get_mut on a shared Arc", worker)
				}
				if _, ok := ArcTryUnwrap(clone); ok {
					t.Errorf("worker %d: try_unwrap on a shared Arc", worker)
				}
				other := weak.Clone()
				if round%3 == 0 {
					other.Drop()
					weak.Drop()
				} else {
					weak.Drop()
					other.Drop()
				}
				upgraded.AsRef().Drop()
				clone.Drop()
				clone.Drop()
			}
		}()
	}
	wait.Wait()
	if strong, weak := ArcStrongCount(root), ArcWeakCount(root); strong != 1 || weak != 0 {
		t.Fatalf("after every handle was dropped: strong %d, weak %d", strong, weak)
	}
	if ArcGetMut(root).IsNone() {
		t.Fatal("the last handle is unique")
	}
	if drops.Load() != 0 {
		t.Fatal("dropped early")
	}
	root.Drop()
	if drops.Load() != 1 {
		t.Fatalf("dropped %d times", drops.Load())
	}
}

func TestArcUpgradeRacingTheLastDrop(t *testing.T) {
	for range 500 {
		canary, drops := newCanary()
		arc := ArcNew(canary)
		weak := ArcDowngrade(arc)
		var upgraded Option[Arc[probeCanary]]
		var wait sync.WaitGroup
		wait.Add(2)
		go func() { defer wait.Done(); upgraded = weak.Upgrade() }()
		go func() { defer wait.Done(); arc.Drop() }()
		wait.Wait()
		if strong, ok := upgraded.Get(); ok {
			// The upgrade won: the value lives until this handle goes.
			if drops.Load() != 0 || strong.Get().label != "canary" {
				t.Fatal("the value was dropped under a successful upgrade")
			}
			strong.Drop()
		}
		if drops.Load() != 1 {
			t.Fatalf("the value was dropped %d times", drops.Load())
		}
		weak.Drop()
	}
}

//go:noinline
func leakClones[T any](arc Arc[T], count int) {
	for range count {
		_ = arc.Clone()
	}
}

//go:noinline
func leakWeaks[T any](arc Arc[T], count int) {
	for range count {
		_ = ArcDowngrade(arc)
	}
}

//go:noinline
func leakLastHandle(drops *atomic.Int64) Weak[probeCanary] {
	arc := ArcNew(probeCanary{drops: drops})
	return ArcDowngrade(arc)
}

func TestArcSafetyNetReleasesHandlesThatWereNeverDropped(t *testing.T) {
	arc := ArcNew(1)
	defer arc.Drop()
	leakClones(arc, 5)
	leakWeaks(arc, 3)
	if ArcStrongCount(arc) != 6 || ArcWeakCount(arc) != 3 {
		t.Fatalf("before collection: strong %d, weak %d", ArcStrongCount(arc), ArcWeakCount(arc))
	}
	waitFor(t, "the leaked handles to release their counts", func() bool {
		return ArcStrongCount(arc) == 1 && ArcWeakCount(arc) == 0
	})
	if ArcGetMut(arc).IsNone() {
		t.Fatal("once the leaked handles are released the Arc is unique again")
	}

	// When the leaked handle was the last, the value's destructor runs.
	drops := &atomic.Int64{}
	weak := leakLastHandle(drops)
	defer weak.Drop()
	waitFor(t, "the value of a leaked last handle to be dropped", func() bool { return drops.Load() == 1 })
	waitFor(t, "the Weak to see no Arc", func() bool { return weak.StrongCount() == 0 })
	if weak.Upgrade().IsSome() {
		t.Fatal("the value is gone")
	}

	// A handle that was dropped, or forgotten, is not released again.
	kept := ArcNew(2)
	defer kept.Drop()
	dropped := kept.Clone()
	dropped.Drop()
	forgotten := kept.Clone()
	Forget(forgotten)
	Forget(ArcDowngrade(kept))
	for range 5 {
		runtime.GC()
		time.Sleep(time.Millisecond)
	}
	if ArcStrongCount(kept) != 2 || ArcWeakCount(kept) != 1 {
		t.Fatalf("after dropping one handle and forgetting two: strong %d, weak %d", ArcStrongCount(kept), ArcWeakCount(kept))
	}
	expectPanic(t, "already dropped", func() { forgotten.Get() })
}

type probeBlowsUp struct{ dropped *atomic.Int64 }

func (p probeBlowsUp) Drop() {
	p.dropped.Add(1)
	panic("destructor failed in the safety net")
}

//go:noinline
func leakBlowsUp(dropped *atomic.Int64) {
	_ = ArcNew(probeBlowsUp{dropped: dropped})
}

func TestArcSafetyNetSendsADestructorPanicToTheHook(t *testing.T) {
	hooked := make(chan *PanicInfo, 1)
	previous := TakePanicHook()
	SetPanicHook(func(info *PanicInfo) {
		if strings.Contains(info.Message, "destructor failed in the safety net") {
			select {
			case hooked <- info:
			default:
			}
			return
		}
		previous(info)
	})
	defer SetPanicHook(previous)

	dropped := &atomic.Int64{}
	leakBlowsUp(dropped)
	waitFor(t, "the destructor of the leaked value", func() bool { return dropped.Load() == 1 })
	select {
	case info := <-hooked:
		if info.Thread != "rs-arc-cleanup" {
			t.Fatalf("the panic was reported on thread %q", info.Thread)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("the panic never reached the hook")
	}
}

type leakRecorder struct {
	mu    sync.Mutex
	leaks []ArcLeak
}

func (r *leakRecorder) record(leak ArcLeak) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.leaks = append(r.leaks, leak)
}

func (r *leakRecorder) snapshot() []ArcLeak {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]ArcLeak(nil), r.leaks...)
}

// The three leaks below are told apart by the lines that create them.
//
//go:noinline
func leakOneOfEach(arc Arc[probeLeaf]) (cloneLine, weakLine, newLine int) {
	_, _, cloneLine, _ = runtime.Caller(0)
	_ = arc.Clone()
	_, _, weakLine, _ = runtime.Caller(0)
	_ = ArcDowngrade(arc)
	_, _, newLine, _ = runtime.Caller(0)
	_ = ArcNew(probeLeaf{Label: "leaked"})
	return cloneLine + 1, weakLine + 1, newLine + 1
}

//go:noinline
func leakThroughClone(holder probeFoo) int {
	_, _, line, _ := runtime.Caller(0)
	_ = Clone(holder)
	return line + 1
}

func TestArcLeakCheckReportsHandlesThatWereNeverDropped(t *testing.T) {
	recorder := &leakRecorder{}
	previousHook := SetArcLeakHook(recorder.record)
	previousCheck := SetArcLeakCheck(true)
	defer SetArcLeakHook(previousHook)
	defer SetArcLeakCheck(previousCheck)

	arc := ArcNew(newProbeLeaf("kept"))
	defer arc.Drop()
	cloneLine, weakLine, newLine := leakOneOfEach(arc)
	holder := probeFoo{inner: ArcNew(int32(1))}
	defer holder.inner.Drop()
	reflectLine := leakThroughClone(holder)

	// Handles that are dropped, taken or forgotten are not leaks.
	tidy := arc.Clone()
	tidy.Drop()
	tidyWeak := ArcDowngrade(arc)
	tidyWeak.Drop()
	ArcTryUnwrap(ArcNew(1))
	ArcIntoInner(ArcNew(2))
	Forget(ArcNew(3))
	moved := ArcNew(4)
	movedWeak := ArcDowngrade(moved)
	ArcMakeMut(&moved)
	moved.Drop()
	movedWeak.Drop()

	waitFor(t, "four leak reports", func() bool { return len(recorder.snapshot()) >= 4 })
	for range 5 {
		runtime.GC()
		time.Sleep(time.Millisecond)
	}
	leaks := recorder.snapshot()
	if len(leaks) != 4 {
		t.Fatalf("got %d leak reports, want 4: %v", len(leaks), leaks)
	}
	find := func(line int) ArcLeak {
		for _, leak := range leaks {
			if leak.Line == line {
				return leak
			}
		}
		t.Fatalf("no leak reported for line %d in %v", line, leaks)
		return ArcLeak{}
	}
	cloneLeak, weakLeak, newLeak, reflectLeak := find(cloneLine), find(weakLine), find(newLine), find(reflectLine)
	if cloneLeak.Handle != "Arc" || weakLeak.Handle != "Weak" || newLeak.Handle != "Arc" || reflectLeak.Handle != "Arc" {
		t.Fatalf("handles: %v", leaks)
	}
	if cloneLeak.Type != "rs.probeLeaf" || reflectLeak.Type != "int32" {
		t.Fatalf("types: %v", leaks)
	}
	for _, leak := range leaks {
		if !strings.HasSuffix(leak.File, "/go/rs/arc_test.go") {
			t.Errorf("the creation site must be the caller's file, got %s", leak.File)
		}
	}
	if !strings.HasSuffix(cloneLeak.Function, ".leakOneOfEach") || !strings.HasSuffix(reflectLeak.Function, ".leakThroughClone") {
		t.Fatalf("functions: %q and %q", cloneLeak.Function, reflectLeak.Function)
	}
	description := weakLeak.String()
	if !strings.HasPrefix(description, "rs.Weak[rs.probeLeaf] created at ") ||
		!strings.HasSuffix(description, "was garbage collected without Drop") || !strings.Contains(description, "arc_test.go:") {
		t.Fatalf("description = %q", description)
	}

	// With the check off nothing is recorded and nothing is reported.
	SetArcLeakCheck(false)
	leakClones(arc, 3)
	waitFor(t, "the unreported leaks to be released", func() bool { return ArcStrongCount(arc) == 1 })
	if got := len(recorder.snapshot()); got != 4 {
		t.Fatalf("leaks were reported with the check off: %d", got)
	}
}

func TestArcLeakCheckIsTurnedOnByTheEnvironment(t *testing.T) {
	if os.Getenv("MJ_RS_LEAK_CHECK_PROBE") == "1" {
		// The child: say what the check starts as.
		if SetArcLeakCheck(false) {
			os.Stdout.WriteString("leak check on\n")
		} else {
			os.Stdout.WriteString("leak check off\n")
		}
		return
	}
	run := func(setting string) string {
		command := exec.Command(os.Args[0], "-test.run=^TestArcLeakCheckIsTurnedOnByTheEnvironment$")
		command.Env = append(os.Environ(), "MJ_RS_LEAK_CHECK_PROBE=1", "MJ_RS_LEAK_CHECK="+setting)
		output, err := command.CombinedOutput()
		if err != nil {
			t.Fatalf("the child test failed: %v\n%s", err, output)
		}
		return string(output)
	}
	if output := run("1"); !strings.Contains(output, "leak check on") {
		t.Fatalf("with MJ_RS_LEAK_CHECK=1: %s", output)
	}
	if output := run("0"); !strings.Contains(output, "leak check off") {
		t.Fatalf("with MJ_RS_LEAK_CHECK=0: %s", output)
	}
}

func TestArcReflectArc(t *testing.T) {
	var slot Arc[probeLeaf]
	var reflected ReflectArc = &slot
	if reflected.ReflectArcType().String() != "rs.probeLeaf" {
		t.Fatalf("type = %s", reflected.ReflectArcType())
	}
	reflected.ReflectArcSet(newProbeLeaf("first"))
	if reflected.ReflectArcGet().(*probeLeaf).Label != "first" || slot.Get().Label != "first" {
		t.Fatal("ReflectArcSet stores Arc::new(value)")
	}
	keeper := slot.Clone()
	defer keeper.Drop()
	reflected.ReflectArcSet(newProbeLeaf("second"))
	if ArcStrongCount(keeper) != 1 || slot.Get().Label != "second" {
		t.Fatal("ReflectArcSet drops the handle it replaces")
	}
	slot.Drop()
}
