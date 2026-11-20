// Port of std 1.99.1 tests/thread_local/tests.rs and dynamic_tests.rs

package rsthread

// Not ported:
//   - fiber_does_not_trigger_dtor: Windows fibers; a goroutine has none.
//
// Rust's `UnsafeCell<Option<T>>` thread-locals are a Go struct holding an
// rs.Option[T], with the Drop that Rust derives for it: it drops the value
// it holds.

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rssync"
)

// localSignal is Signal: a flag behind a mutex and a condition variable.
type localSignal struct {
	set  rssync.Mutex[bool]
	cvar rssync.Condvar
}

func localSignalDefault() *localSignal {
	return &localSignal{set: rssync.MutexNew(false), cvar: rssync.CondvarNew()}
}

func (s *localSignal) notify() {
	s.set.With(func(set *bool) { *set = true })
	s.cvar.NotifyOne()
}

func (s *localSignal) wait() {
	set := s.set.Lock()
	defer set.Drop()
	for !*set.Get() {
		s.cvar.Wait(set)
	}
}

func (s *localSignal) isSet() bool {
	set := s.set.Lock()
	defer set.Drop()
	return *set.Get()
}

// localNotifyOnDrop is NotifyOnDrop.
type localNotifyOnDrop struct{ signal *localSignal }

func (n *localNotifyOnDrop) Drop() { n.signal.notify() }

// localNotifySlot is `UnsafeCell<Option<NotifyOnDrop>>`.
type localNotifySlot struct{ value rs.Option[*localNotifyOnDrop] }

func (s *localNotifySlot) Drop() {
	if value, ok := s.value.Get(); ok {
		value.Drop()
	}
}

func TestLocal_smoke_no_dtor(t *testing.T) {
	run := func(key *LocalKey[rs.Cell[int32]]) {
		key.With(func(f *rs.Cell[int32]) {
			assertEq(t, f.Get(), 1)
			f.Set(2)
		})
		thread := Spawn(func() rs.Unit {
			key.With(func(f *rs.Cell[int32]) {
				if f.Get() != 1 {
					panic("assertion `left == right` failed")
				}
			})
			return rs.Unit{}
		})
		thread.JoinUnwrap()

		key.With(func(f *rs.Cell[int32]) {
			assertEq(t, f.Get(), 2)
		})
	}
	foo := LocalKeyNew(func() rs.Cell[int32] { return rs.CellNew[int32](1) })
	run(foo)
	foo2 := LocalKeyNew(func() rs.Cell[int32] { return rs.CellNew[int32](1) })
	run(foo2)
}

// localFoo is the Foo of `states`: its destructor finds its own key
// destroyed.
type localFoo struct {
	key     *LocalKey[localFoo]
	checked *rssync.AtomicBool
}

func (f *localFoo) Drop() {
	_, ok := LocalKeyTryWith(f.key, func(*localFoo) rs.Unit { return rs.Unit{} })
	if ok {
		panic("assertion failed: self.0.try_with(|_| ()).is_err()")
	}
	f.checked.Store(true, rssync.Ordering_SeqCst)
}

func TestLocal_states(t *testing.T) {
	run := func(foo *LocalKey[localFoo], checked *rssync.AtomicBool) {
		Spawn(func() rs.Unit {
			_, ok := LocalKeyTryWith(foo, func(*localFoo) rs.Unit { return rs.Unit{} })
			if !ok {
				panic("assertion failed: foo.try_with(|_| ()).is_ok()")
			}
			return rs.Unit{}
		}).JoinUnwrap()
	}
	var checked, checked2 rssync.AtomicBool
	var foo, foo2 *LocalKey[localFoo]
	foo = LocalKeyNew(func() localFoo { return localFoo{key: foo, checked: &checked} })
	run(foo, &checked)
	foo2 = LocalKeyNew(func() localFoo { return localFoo{key: foo2, checked: &checked2} })
	run(foo2, &checked2)
	assert(t, checked.Load(rssync.Ordering_SeqCst) && checked2.Load(rssync.Ordering_SeqCst), "the destructors ran")
}

func TestLocal_smoke_dtor(t *testing.T) {
	run := func(key *LocalKey[localNotifySlot]) {
		signal := localSignalDefault()
		signal2 := signal
		thread := Spawn(func() rs.Unit {
			signal := rs.Some(signal2)
			key.With(func(f *localNotifySlot) {
				f.value = rs.Some(&localNotifyOnDrop{signal: signal.Take().Unwrap()})
			})
			return rs.Unit{}
		})
		signal.wait()
		assert(t, signal.isSet())
		thread.JoinUnwrap()
	}
	foo := LocalKeyNew(func() localNotifySlot { return localNotifySlot{} })
	run(foo)
	foo2 := LocalKeyNew(func() localNotifySlot { return localNotifySlot{} })
	run(foo2)
}

// The keys and types of `circular`.
type (
	localS1 struct {
		k1 *LocalKey[localS1Slot]
		k2 *LocalKey[localS2Slot]
	}
	localS2 struct {
		k1 *LocalKey[localS1Slot]
		k2 *LocalKey[localS2Slot]
	}
	localS1Slot struct{ value rs.Option[*localS1] }
	localS2Slot struct{ value rs.Option[*localS2] }
)

var localCircularHits int

func (s *localS1Slot) Drop() {
	if value, ok := s.value.Get(); ok {
		value.Drop()
	}
}

func (s *localS2Slot) Drop() {
	if value, ok := s.value.Get(); ok {
		value.Drop()
	}
}

func (s *localS1) Drop() {
	localCircularHits++
	if _, ok := LocalKeyTryWith(s.k2, func(*localS2Slot) rs.Unit { return rs.Unit{} }); !ok {
		if localCircularHits != 3 {
			panic("assertion `left == right` failed: HITS == 3")
		}
	} else {
		if localCircularHits == 1 {
			s.k2.With(func(slot *localS2Slot) { slot.value = rs.Some(&localS2{k1: s.k1, k2: s.k2}) })
		} else if localCircularHits != 3 {
			panic("assertion `left == right` failed: HITS == 3")
		}
	}
}

func (s *localS2) Drop() {
	localCircularHits++
	if _, ok := LocalKeyTryWith(s.k1, func(*localS1Slot) rs.Unit { return rs.Unit{} }); !ok {
		panic("assertion failed: self.0.try_with(|_| ()).is_ok()")
	}
	if localCircularHits != 2 {
		panic("assertion `left == right` failed: HITS == 2")
	}
	s.k1.With(func(slot *localS1Slot) { slot.value = rs.Some(&localS1{k1: s.k1, k2: s.k2}) })
}

func TestLocal_circular(t *testing.T) {
	k1 := LocalKeyNew(func() localS1Slot { return localS1Slot{} })
	k2 := LocalKeyNew(func() localS2Slot { return localS2Slot{} })
	k3 := LocalKeyNew(func() localS1Slot { return localS1Slot{} })
	k4 := LocalKeyNew(func() localS2Slot { return localS2Slot{} })

	localCircularHits = 0
	Spawn(func() rs.Unit {
		(&localS1{k1: k1, k2: k2}).Drop()
		return rs.Unit{}
	}).JoinUnwrap()
	assertEq(t, localCircularHits, 3)

	localCircularHits = 0

	Spawn(func() rs.Unit {
		(&localS1{k1: k3, k2: k4}).Drop()
		return rs.Unit{}
	}).JoinUnwrap()
	assertEq(t, localCircularHits, 3)
}

// localSelfRef is the S1 of `self_referential`.
type (
	localSelfRef     struct{ key *LocalKey[localSelfRefSlot] }
	localSelfRefSlot struct{ value rs.Option[*localSelfRef] }
)

func (s *localSelfRefSlot) Drop() {
	if value, ok := s.value.Get(); ok {
		value.Drop()
	}
}

func (s *localSelfRef) Drop() {
	if _, ok := LocalKeyTryWith(s.key, func(*localSelfRefSlot) rs.Unit { return rs.Unit{} }); ok {
		panic("assertion failed: self.0.try_with(|_| ()).is_err()")
	}
}

func TestLocal_self_referential(t *testing.T) {
	var k1, k2 *LocalKey[localSelfRefSlot]
	k1 = LocalKeyNew(func() localSelfRefSlot { return localSelfRefSlot{} })
	k2 = LocalKeyNew(func() localSelfRefSlot { return localSelfRefSlot{} })

	Spawn(func() rs.Unit {
		k1.With(func(s *localSelfRefSlot) { s.value = rs.Some(&localSelfRef{key: k1}) })
		return rs.Unit{}
	}).JoinUnwrap()

	Spawn(func() rs.Unit {
		k2.With(func(s *localSelfRefSlot) { s.value = rs.Some(&localSelfRef{key: k2}) })
		return rs.Unit{}
	}).JoinUnwrap()
}

// The S1 of dtors_in_dtors_in_dtors: its destructor stores a NotifyOnDrop in
// another thread-local, which is then dropped too.
type (
	localDtorS1 struct {
		signal *localSignal
		k2     *LocalKey[localNotifySlot]
	}
	localDtorS1Slot struct{ value rs.Option[*localDtorS1] }
)

func (s *localDtorS1Slot) Drop() {
	if value, ok := s.value.Get(); ok {
		value.Drop()
	}
}

func (s *localDtorS1) Drop() {
	_, _ = LocalKeyTryWith(s.k2, func(slot *localNotifySlot) rs.Unit {
		slot.value = rs.Some(&localNotifyOnDrop{signal: s.signal})
		return rs.Unit{}
	})
}

// Note that this test will deadlock if TLS destructors aren't run (this
// requires the destructor to be run to pass the test).
func TestLocal_dtors_in_dtors_in_dtors(t *testing.T) {
	k1 := LocalKeyNew(func() localDtorS1Slot { return localDtorS1Slot{} })
	k2 := LocalKeyNew(func() localNotifySlot { return localNotifySlot{} })

	signal := localSignalDefault()
	signal2 := signal
	thread := Spawn(func() rs.Unit {
		signal := rs.Some(signal2)
		k1.With(func(s *localDtorS1Slot) {
			s.value = rs.Some(&localDtorS1{signal: signal.Take().Unwrap(), k2: k2})
		})
		return rs.Unit{}
	})
	defer thread.Drop()
	signal.wait()
}

func TestLocal_dtors_in_dtors_in_dtors_const_init(t *testing.T) {
	k1 := LocalKeyNew(func() localDtorS1Slot { return localDtorS1Slot{} })
	k2 := LocalKeyNew(func() localNotifySlot { return localNotifySlot{} })

	signal := localSignalDefault()
	signal2 := signal
	thread := Spawn(func() rs.Unit {
		signal := rs.Some(signal2)
		k1.With(func(s *localDtorS1Slot) {
			s.value = rs.Some(&localDtorS1{signal: signal.Take().Unwrap(), k2: k2})
		})
		return rs.Unit{}
	})
	defer thread.Drop()
	signal.wait()
}

// The states of join_orders_after_tls_destructors' rendezvous.
const (
	localFresh                uint8 = 0
	localThread2Launched      uint8 = 1
	localThread1Waiting       uint8 = 2
	localMainThreadRendezvous uint8 = 3
	localThread2Joined        uint8 = 4
)

var localSyncState = rssync.AtomicU8New(localFresh)

// localTlDrop is TlDrop.
type localTlDrop struct{}

func (localTlDrop) Drop() {
	syncState := localSyncState.Swap(localThread1Waiting, rssync.Ordering_Relaxed)
	for {
		switch syncState {
		case localThread2Launched, localThread1Waiting:
			YieldNow()
		case localMainThreadRendezvous:
			return
		case localThread2Joined:
			panic(rs.Panicf("Thread 1 still running after thread 2 joined on thread 1"))
		default:
			panic(rs.Unreachable("sync state: {}", syncState))
		}
		syncState = localSyncState.Load(rssync.Ordering_Relaxed)
	}
}

var localTlDropKey = LocalKeyNew(func() localTlDrop { return localTlDrop{} })

// This test tests that TLS destructors have run before the thread joins. The
// test has no false positives (meaning: if the test fails, there's actually
// an ordering problem). It may have false negatives, where the test passes but
// join is not guaranteed to be after the TLS destructors. However, false
// negatives should be exceedingly rare due to judicious use of
// thread::yield_now and running the test several times.
func TestLocal_join_orders_after_tls_destructors(t *testing.T) {
	// We emulate a synchronous MPSC rendezvous channel using only atomics and
	// thread::yield_now. We can't use std::mpsc as the implementation itself
	// may rely on thread locals.
	//
	// The basic state machine for an SPSC rendezvous channel is:
	//           FRESH -> THREAD1_WAITING -> MAIN_THREAD_RENDEZVOUS
	// where the first transition is done by the “receiving” thread and the 2nd
	// transition is done by the “sending” thread.
	//
	// We add an additional state `THREAD2_LAUNCHED` between `FRESH` and
	// `THREAD1_WAITING` to block until all threads are actually running.
	//
	// A thread that joins on the “receiving” thread completion should never
	// observe the channel in the `THREAD1_WAITING` state. If this does occur,
	// we switch to the “poison” state `THREAD2_JOINED` and panic all around.
	// (This is equivalent to “sending” from an alternate producer thread.)
	//
	// Relaxed memory ordering is fine because and spawn()/join() already provide all the
	// synchronization we need here.
	for range 10 {
		localSyncState.Store(localFresh, rssync.Ordering_Relaxed)

		jh, err := BuilderSpawn(BuilderNew().Name("thread1"), func() rs.Unit {
			localTlDropKey.With(func(*localTlDrop) {})

			for {
				switch state := localSyncState.Load(rssync.Ordering_Relaxed); state {
				case localFresh:
					YieldNow()
					continue
				case localThread2Launched:
				default:
					panic(rs.Unreachable("sync state: {}", state))
				}
				break
			}
			return rs.Unit{}
		})
		assert(t, err == nil)

		jh2, err := BuilderSpawn(BuilderNew().Name("thread2"), func() rs.Unit {
			if state := localSyncState.Swap(localThread2Launched, rssync.Ordering_Relaxed); state != localFresh {
				panic("assertion `left == right` failed")
			}
			jh.JoinUnwrap()
			switch state := localSyncState.Swap(localThread2Joined, rssync.Ordering_Relaxed); state {
			case localMainThreadRendezvous:
				return rs.Unit{}
			case localThread2Launched, localThread1Waiting:
				panic(rs.Panicf("Thread 2 running after thread 1 join before main thread rendezvous"))
			default:
				panic(rs.Unreachable("sync state: {:?}", state))
			}
		})
		assert(t, err == nil)

	rendezvous:
		for {
			switch state, ok := localSyncState.CompareExchange(
				localThread1Waiting,
				localMainThreadRendezvous,
				rssync.Ordering_Relaxed,
				rssync.Ordering_Relaxed,
			); {
			case ok:
				break rendezvous
			case state == localFresh:
				YieldNow()
			case state == localThread2Launched:
				YieldNow()
			case state == localThread2Joined:
				t.Fatal("Main thread rendezvous after thread 2 joined thread 1")
			default:
				t.Fatalf("sync state: %d", state)
			}
		}
		jh2.JoinUnwrap()
	}
}

// The thread-locals of thread_current_in_dtor.
type (
	localDefer        struct{}
	localRetrieveName struct{}
)

var (
	localName     = rssync.MutexNew(rs.None[string]())
	localRetrieve = LocalKeyNew(func() localRetrieveName { return localRetrieveName{} })
	localDeferKey = LocalKeyNew(func() localDefer { return localDefer{} })
)

func (localDefer) Drop() {
	localRetrieve.With(func(*localRetrieveName) {})
}

func (localRetrieveName) Drop() {
	localName.With(func(name *rs.Option[string]) { *name = rs.Some(Current().Name().Unwrap()) })
}

// Test that thread::current is still available in TLS destructors.
func TestLocal_thread_current_in_dtor(t *testing.T) {
	// Go through one round of TLS destruction first.
	handle, err := BuilderSpawn(BuilderNew().Name("test"), func() rs.Unit {
		localDeferKey.With(func(*localDefer) {})
		return rs.Unit{}
	})
	assert(t, err == nil)
	handle.JoinUnwrap()
	name := localName.Lock()
	defer name.Drop()
	assertEq(t, name.Get().Unwrap(), "test")
}

func TestLocalDynamic_smoke(t *testing.T) {
	square := func(i int32) int32 { return i * i }
	foo := LocalKeyNew(func() int32 { return square(3) })

	foo.With(func(f *int32) {
		assertEq(t, *f, 9)
	})
}

func TestLocalDynamic_hashmap(t *testing.T) {
	makeMap := func() rs.RefCell[map[int32]int32] {
		m := map[int32]int32{}
		m[1] = 2
		return rs.RefCellNew(m)
	}
	foo := LocalKeyNew(makeMap)

	foo.With(func(m *rs.RefCell[map[int32]int32]) {
		assertEq(t, (*m.Borrow())[1], 2)
	})
}

func TestLocalDynamic_refcell_vec(t *testing.T) {
	foo := LocalKeyNew(func() rs.RefCell[[]uint32] { return rs.RefCellNew([]uint32{1, 2, 3}) })

	foo.With(func(vec *rs.RefCell[[]uint32]) {
		assertEq(t, len(*vec.Borrow()), 3)
		*vec.BorrowMut() = append(*vec.BorrowMut(), 4)
		assertEq(t, (*vec.Borrow())[3], 4)
	})
}
