// Port of std 1.99.1 src/thread/tests.rs

package rsthread

// Not ported:
//   - test_named_thread_truncation: thread names are not given to the
//     operating system.
//   - test_avoid_copying_the_body_spawn, _thread_spawn, _join: they compare
//     the address of a boxed value in the parent and the child; a Go closure
//     captures by reference, so there is nothing to copy.
//   - test_size_of_option_thread_id: memory layout.
//   - test_thread_os_id_not_equal: thread::current_os_id is unstable.
//   - scope_join_race: runs only under Miri's data race detector.

import (
	"math"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rssync"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// !!! These tests are dangerous. If something is buggy, they will hang, !!!
// !!! instead of exiting cleanly. This might wedge the buildbots.       !!!

func TestThread_test_unnamed_thread(t *testing.T) {
	_, caught := Spawn(func() rs.Unit {
		if Current().Name().IsSome() {
			panic("assertion failed: thread::current().name().is_none()")
		}
		return rs.Unit{}
	}).Join()
	assert(t, caught == nil, "thread panicked")
}

func TestThread_test_named_thread(t *testing.T) {
	handle, err := BuilderSpawn(BuilderNew().Name("ada lovelace"), func() rs.Unit {
		if Current().Name().Unwrap() != "ada lovelace" {
			panic("assertion failed: thread::current().name().unwrap() == \"ada lovelace\".to_string()")
		}
		return rs.Unit{}
	})
	assert(t, err == nil)
	handle.JoinUnwrap()
}

func TestThread_test_invalid_named_thread(t *testing.T) {
	shouldPanic(t, "", func() {
		_, _ = BuilderSpawn(BuilderNew().Name("ada l\x00velace"), func() rs.Unit { return rs.Unit{} })
	})
}

func TestThread_test_run_basic(t *testing.T) {
	tx, rx := rssync.MpscChannel[rs.Unit]()
	Spawn(func() rs.Unit {
		defer tx.Drop()
		rs.UnwrapUnit(tx.Send(rs.Unit{}))
		return rs.Unit{}
	})
	rs.Unwrap(rx.Recv())
}

func TestThread_test_is_finished(t *testing.T) {
	b := rssync.BarrierNew(2)
	thread := Spawn(func() int {
		b.Wait()
		return 1234
	})

	// Thread is definitely running here, since it's still waiting for the barrier.
	assertEq(t, thread.IsFinished(), false)

	// Unblock the barrier.
	b.Wait()

	// Now check that t.is_finished() becomes true within a reasonable time.
	start := rstime.InstantNow()
	for !thread.IsFinished() {
		assert(t, start.Elapsed() < rstime.DurationFromSecs(2))
		Sleep(rstime.DurationFromMillis(15))
	}

	// Joining the thread should not block for a significant time now.
	joinTime := rstime.InstantNow()
	assertEq(t, thread.JoinUnwrap(), 1234)
	assert(t, joinTime.Elapsed() < rstime.DurationFromSecs(2))
}

func TestThread_test_join_panic(t *testing.T) {
	quietPanics(t)
	if _, caught := Spawn(func() rs.Unit { panic(rs.Panicf("explicit panic")) }).Join(); caught == nil {
		t.Fatal("the thread did not panic")
	}
}

func threadSpawnSched(i int32, tx rssync.MpscSender[rs.Unit]) {
	tx2 := tx.Clone()
	Spawn(func() rs.Unit {
		defer tx2.Drop()
		if i == 0 {
			rs.UnwrapUnit(tx2.Send(rs.Unit{}))
		} else {
			threadSpawnSched(i-1, tx2)
		}
		return rs.Unit{}
	})
}

func TestThread_test_spawn_sched(t *testing.T) {
	tx, rx := rssync.MpscChannel[rs.Unit]()
	threadSpawnSched(10, tx)
	tx.Drop()
	rs.Unwrap(rx.Recv())
}

func TestThread_test_spawn_sched_childs_on_default_sched(t *testing.T) {
	tx, rx := rssync.MpscChannel[rs.Unit]()

	Spawn(func() rs.Unit {
		Spawn(func() rs.Unit {
			defer tx.Drop()
			rs.UnwrapUnit(tx.Send(rs.Unit{}))
			return rs.Unit{}
		})
		return rs.Unit{}
	})

	rs.Unwrap(rx.Recv())
}

func TestThread_test_child_doesnt_ref_parent(t *testing.T) {
	// If the child refcounts the parent thread, this will stack overflow when
	// climbing the thread tree to dereference each ancestor. (See #1789)
	// (well, it would if the constant were 8000+ - I lowered it to be more
	// valgrind-friendly. try this at home, instead..!)
	const generations = 16
	var childNo func(x uint32) func()
	childNo = func(x uint32) func() {
		return func() {
			if x < generations {
				Spawn(func() rs.Unit { childNo(x + 1)(); return rs.Unit{} })
			}
		}
	}
	Spawn(func() rs.Unit { childNo(0)(); return rs.Unit{} })
}

func TestThread_test_simple_newsched_spawn(t *testing.T) {
	Spawn(func() rs.Unit { return rs.Unit{} })
}

func TestThread_test_try_panic_message_string_literal(t *testing.T) {
	quietPanics(t)
	_, caught := Spawn(func() rs.Unit {
		panic(rs.Panicf("static string"))
	}).Join()
	if caught == nil {
		t.Fatal("the thread did not panic")
	}
	assertEq(t, caught.Message, "static string")
}

func TestThread_test_try_panic_any_message_owned_str(t *testing.T) {
	quietPanics(t)
	_, caught := Spawn(func() rs.Unit {
		panic("owned string")
	}).Join()
	if caught == nil {
		t.Fatal("the thread did not panic")
	}
	payload, ok := caught.Payload.(string)
	assert(t, ok)
	assertEq(t, payload, "owned string")
}

func TestThread_test_try_panic_any_message_any(t *testing.T) {
	quietPanics(t)
	_, caught := Spawn(func() rs.Unit {
		panic(any(uint16(413)))
	}).Join()
	if caught == nil {
		t.Fatal("the thread did not panic")
	}
	payload, ok := caught.Payload.(uint16)
	assert(t, ok)
	assertEq(t, payload, 413)
}

type threadJuju struct{}

func TestThread_test_try_panic_any_message_unit_struct(t *testing.T) {
	quietPanics(t)
	_, caught := Spawn(func() rs.Unit { panic(threadJuju{}) }).Join()
	if caught == nil {
		t.Fatal("the thread did not panic")
	}
	_, ok := caught.Payload.(threadJuju)
	assert(t, ok)
}

func TestThread_test_park_unpark_before(t *testing.T) {
	for range 10 {
		Current().Unpark()
		Park()
	}
}

func TestThread_test_park_unpark_called_other_thread(t *testing.T) {
	for range 10 {
		th := Current()

		// Here we rely on `thread::spawn` (specifically the part that runs after spawning
		// the thread) to not consume the parking token.
		guard := Spawn(func() rs.Unit {
			Sleep(rstime.DurationFromMillis(50))
			th.Unpark()
			return rs.Unit{}
		})
		defer guard.Drop()

		Park()
	}
}

func TestThread_test_park_timeout_unpark_before(t *testing.T) {
	for range 10 {
		Current().Unpark()
		ParkTimeout(rstime.DurationFromMillis(math.MaxUint32))
	}
}

func TestThread_test_park_timeout_unpark_not_called(t *testing.T) {
	for range 10 {
		ParkTimeout(rstime.DurationFromMillis(10))
	}
}

func TestThread_test_park_timeout_unpark_called_other_thread(t *testing.T) {
	for range 10 {
		th := Current()

		// Here we rely on `thread::spawn` (specifically the part that runs after spawning
		// the thread) to not consume the parking token.
		guard := Spawn(func() rs.Unit {
			Sleep(rstime.DurationFromMillis(50))
			th.Unpark()
			return rs.Unit{}
		})
		defer guard.Drop()

		ParkTimeout(rstime.DurationFromMillis(math.MaxUint32))
	}
}

func TestThread_sleep_ms_smoke(t *testing.T) {
	Sleep(rstime.DurationFromMillis(2))
}

func TestThread_test_thread_id_equal(t *testing.T) {
	assert(t, Current().Id() == Current().Id())
}

func TestThread_test_thread_id_not_equal(t *testing.T) {
	spawnedId := Spawn(func() ThreadId { return Current().Id() }).JoinUnwrap()
	assert(t, Current().Id() != spawnedId)
}

// threadX is the X of test_scoped_threads_drop_result_before_join: a value
// whose destructor spawns another scoped thread.
type threadX struct {
	scope            *ScopeHandle
	actuallyFinished *rssync.AtomicBool
}

func (x *threadX) Drop() {
	Sleep(rstime.DurationFromMillis(20))
	actuallyFinished := x.actuallyFinished
	ScopeSpawn(x.scope, func() rs.Unit {
		Sleep(rstime.DurationFromMillis(20))
		actuallyFinished.Store(true, rssync.Ordering_Relaxed)
		return rs.Unit{}
	}).Drop()
}

func TestThread_test_scoped_threads_drop_result_before_join(t *testing.T) {
	actuallyFinished := new(rssync.AtomicBool)
	Scope(func(s *ScopeHandle) rs.Unit {
		ScopeSpawn(s, func() *threadX {
			Sleep(rstime.DurationFromMillis(20))
			return &threadX{scope: s, actuallyFinished: actuallyFinished}
		}).Drop()
		return rs.Unit{}
	})
	assert(t, actuallyFinished.Load(rssync.Ordering_Relaxed))
}

func TestThread_test_scoped_threads_nll(t *testing.T) {
	// this is mostly a *compilation test* for this exact function:
	foo := func(x *uint8) {
		Scope(func(s *ScopeHandle) rs.Unit {
			ScopeSpawn(s, func() rs.Unit {
				_ = *x
				return rs.Unit{}
			})
			return rs.Unit{}
		})
	}
	// let's also run it for good measure
	x := uint8(42)
	foo(&x)
}

// Test that the smallest value for stack_size works on Windows.
func TestThread_test_minimal_thread_stack(t *testing.T) {
	if !rs.CfgWindows {
		t.Skip("#[cfg(windows)]")
	}
	count := new(rssync.AtomicU8)

	builder := BuilderNew().StackSize(1)
	handle, err := BuilderSpawn(builder, func() uint8 { return count.FetchAdd(1, rssync.Ordering_Relaxed) })
	assert(t, err == nil)
	before := handle.JoinUnwrap()
	assertEq(t, before, 0)
	assertEq(t, count.Load(rssync.Ordering_Relaxed), 1)
}

// What the port adds to Rust's behaviour.

func TestThread_port_panicking_follows_the_goroutines_panic(t *testing.T) {
	assert(t, !Panicking())
	var during, nested, afterRecovery bool
	func() {
		defer func() { recover() }()
		defer func() {
			during = Panicking()
			nested = func() bool { return Panicking() }()
		}()
		panic("unwinding")
	}()
	afterRecovery = Panicking()
	assert(t, during, "in a deferred call during a panic")
	assert(t, nested, "in a function called by that deferred call")
	assert(t, !afterRecovery, "after the panic was recovered")

	var underGoexit bool
	done := make(chan struct{})
	go func() {
		defer close(done)
		defer func() { underGoexit = Panicking() }()
		runtime.Goexit()
	}()
	<-done
	assert(t, !underGoexit, "runtime.Goexit is not a panic")

	var inHook bool
	rs.SetPanicHook(func(*rs.PanicInfo) { inHook = Panicking() })
	defer rs.TakePanicHook()
	_, caught := Spawn(func() rs.Unit { panic("x") }).Join()
	assert(t, caught != nil)
	assert(t, inHook, "the panic hook runs while panicking")
}

func TestThread_port_a_thread_that_ends_through_goexit_is_joined_as_a_panic(t *testing.T) {
	quietPanics(t)
	_, caught := Spawn(func() int {
		runtime.Goexit()
		return 1
	}).Join()
	if caught == nil {
		t.Fatal("Join returned a result for a thread that never returned")
	}
	assert(t, strings.Contains(caught.Message, "runtime.Goexit"), caught.Message)
}

func TestThread_port_the_hook_names_the_thread(t *testing.T) {
	var names []string
	rs.SetPanicHook(func(info *rs.PanicInfo) { names = append(names, info.Thread) })
	defer rs.TakePanicHook()
	named, _ := BuilderSpawn(BuilderNew().Name("worker"), func() rs.Unit { panic("x") })
	named.Join()
	Spawn(func() rs.Unit { panic("y") }).Join()
	// An unnamed thread is "" to the hook, which prints "<unnamed>".
	assertEq(t, strings.Join(names, ","), "worker,")
}

func TestThread_port_threads_and_their_ids(t *testing.T) {
	// The main goroutine is the main thread, with id 1.
	assertEq(t, Thread{inner: mainThread}.Name().Unwrap(), "main")
	assertEq(t, Thread{inner: mainThread}.Id(), ThreadId(1))
	// A test goroutine is an unnamed thread of its own, whose id does not
	// change.
	assert(t, Current().Name().IsNone())
	first := Current().Id()
	assertEq(t, Current().Id(), first)
	assertNe(t, first, ThreadId(1))
	other := make(chan ThreadId)
	go func() { other <- Current().Id() }()
	assertNe(t, <-other, first)
	// A spawned thread takes the next id when it is spawned.
	handle := Spawn(func() ThreadId { return Current().Id() })
	assertEq(t, handle.Thread().Id(), handle.JoinUnwrap())
	assert(t, handle.Thread().Id() > first)
}

func TestThread_port_scope_waits_for_threads_nobody_joined(t *testing.T) {
	var finished rssync.AtomicUsize
	Scope(func(s *ScopeHandle) rs.Unit {
		for range 5 {
			ScopeSpawn(s, func() rs.Unit {
				Sleep(rstime.DurationFromMillis(10))
				finished.FetchAdd(1, rssync.Ordering_SeqCst)
				return rs.Unit{}
			})
		}
		return rs.Unit{}
	})
	assertEq(t, finished.Load(rssync.Ordering_SeqCst), 5)
}

func TestThread_port_scope_panics_only_for_a_panic_nobody_joined(t *testing.T) {
	quietPanics(t)
	// Joined: the scope does not panic.
	got := Scope(func(s *ScopeHandle) bool {
		_, caught := ScopeSpawn(s, func() rs.Unit { panic("joined") }).Join()
		return caught != nil
	})
	assert(t, got)
	// Not joined: it does, once the thread has ended.
	var ended rssync.AtomicBool
	message := panicMessage(t, func() {
		Scope(func(s *ScopeHandle) rs.Unit {
			ScopeSpawn(s, func() rs.Unit {
				Sleep(rstime.DurationFromMillis(10))
				ended.Store(true, rssync.Ordering_SeqCst)
				panic("not joined")
			})
			return rs.Unit{}
		})
	})
	assertEq(t, message, "a scoped thread panicked")
	assert(t, ended.Load(rssync.Ordering_SeqCst))
	// A handle moved into another scoped thread and joined there counts as
	// joined.
	Scope(func(s *ScopeHandle) rs.Unit {
		first := ScopeSpawn(s, func() rs.Unit { panic("joined elsewhere") })
		ScopeSpawn(s, func() bool {
			_, caught := first.Join()
			return caught != nil
		})
		return rs.Unit{}
	})
}

func TestThread_port_scope_resumes_its_closures_panic_after_waiting(t *testing.T) {
	quietPanics(t)
	var ended rssync.AtomicBool
	defer func() {
		recovered := rs.PanicInfoOf(recover())
		assertEq(t, recovered.Message, "in the closure")
		assert(t, ended.Load(rssync.Ordering_SeqCst), "the scope returned before its thread ended")
	}()
	Scope(func(s *ScopeHandle) rs.Unit {
		ScopeSpawn(s, func() rs.Unit {
			Sleep(rstime.DurationFromMillis(20))
			ended.Store(true, rssync.Ordering_SeqCst)
			return rs.Unit{}
		})
		panic(rs.Panicf("in the closure"))
	})
}

func TestThread_port_join_unwrap_and_expect(t *testing.T) {
	quietPanics(t)
	assertEq(t, Spawn(func() int { return 3 }).JoinUnwrap(), 3)
	assertEq(t, panicMessage(t, func() { Spawn(func() int { panic("x") }).JoinUnwrap() }),
		"called `Result::unwrap()` on an `Err` value: Any { .. }")
	assertEq(t, panicMessage(t, func() { Spawn(func() int { panic("x") }).JoinExpect("the thread finishes") }),
		"the thread finishes: Any { .. }")
	assertEq(t, panicMessage(t, func() {
		Scope(func(s *ScopeHandle) int { return ScopeSpawn(s, func() int { panic("x") }).JoinUnwrap() })
	}), "called `Result::unwrap()` on an `Err` value: Any { .. }")
}

func TestThread_port_a_handle_joins_once(t *testing.T) {
	handle := Spawn(func() int { return 1 })
	handle.JoinUnwrap()
	shouldPanic(t, "used after Join or Drop", func() { handle.Join() })
	dropped := Spawn(func() int { return 1 })
	dropped.Drop()
	dropped.Drop()
	shouldPanic(t, "used after Join or Drop", func() { dropped.Join() })
}

// threadDropCounter counts the results that were dropped.
type threadDropCounter struct{ drops *rssync.AtomicUsize }

func (c threadDropCounter) Drop() { c.drops.FetchAdd(1, rssync.Ordering_SeqCst) }

func TestThread_port_a_result_nobody_joined_is_dropped(t *testing.T) {
	var drops rssync.AtomicUsize
	// Dropped before the thread ends: dropped by the thread.
	handle := Spawn(func() threadDropCounter {
		Sleep(rstime.DurationFromMillis(10))
		return threadDropCounter{drops: &drops}
	})
	handle.Drop()
	for drops.Load(rssync.Ordering_SeqCst) == 0 {
		Sleep(rstime.DurationFromMillis(1))
	}
	// Dropped after it ended: dropped by Drop.
	handle = Spawn(func() threadDropCounter { return threadDropCounter{drops: &drops} })
	for !handle.IsFinished() {
		Sleep(rstime.DurationFromMillis(1))
	}
	handle.Drop()
	assertEq(t, drops.Load(rssync.Ordering_SeqCst), 2)
	// Joined: the result goes to the caller, not dropped.
	Spawn(func() threadDropCounter { return threadDropCounter{drops: &drops} }).JoinUnwrap()
	assertEq(t, drops.Load(rssync.Ordering_SeqCst), 2)
}

func TestThread_port_available_parallelism_is_a_cpu_count(t *testing.T) {
	n, err := AvailableParallelism()
	assert(t, err == nil, err)
	assert(t, n >= 1 && n <= runtime.NumCPU(), n, runtime.NumCPU())
}

func TestThread_port_a_name_with_a_nul_byte_panics_with_rusts_text(t *testing.T) {
	assertEq(t, panicMessage(t, func() {
		BuilderSpawn(BuilderNew().Name("ab\x00cd"), func() rs.Unit { return rs.Unit{} })
	}), "thread name may not contain interior null bytes: NulError(2, [97, 98, 0, 99, 100])")
}

func TestThread_port_negative_durations_are_refused(t *testing.T) {
	shouldPanic(t, "rsthread: negative Duration", func() { Sleep(-time.Nanosecond) })
	shouldPanic(t, "rsthread: negative Duration", func() { ParkTimeout(-time.Nanosecond) })
}
