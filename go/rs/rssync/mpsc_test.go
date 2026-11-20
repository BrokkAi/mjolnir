// Port of std 1.99.1 tests/sync/mpsc.rs
//
// A value Rust moves into a thread's closure is dropped when the closure
// ends; here that is a deferred Drop inside the closure. `let _t =
// thread::spawn(..)` keeps the handle to the end of the test, which joins
// nothing: the thread is detached either way.

package rssync

import (
	"iter"
	"math"
	"os"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsthread"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

func mpscStressFactor() int {
	if value, ok := os.LookupEnv("RUST_TEST_STRESS"); ok {
		factor, err := strconv.Atoi(value)
		if err != nil {
			panic(err)
		}
		return factor
	}
	return 1
}

// mpscSpawn is `thread::spawn(move || ..)` for a closure with no result.
func mpscSpawn(f func()) *rsthread.JoinHandle[rs.Unit] {
	return rsthread.Spawn(func() rs.Unit {
		f()
		return rs.Unit{}
	})
}

func TestMpsc_smoke(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rs.UnwrapUnit(tx.Send(1))
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
}

func TestMpsc_drop_full(t *testing.T) {
	tx, rx := MpscChannel[*int]()
	defer rx.Drop()
	rs.UnwrapUnit(tx.Send(new(int)))
}

func TestMpsc_drop_full_shared(t *testing.T) {
	tx, rx := MpscChannel[*int]()
	defer rx.Drop()
	tx.Clone().Drop()
	tx.Clone().Drop()
	rs.UnwrapUnit(tx.Send(new(int)))
}

func TestMpsc_smoke_shared(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rs.UnwrapUnit(tx.Send(1))
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
	tx2 := tx.Clone()
	rs.UnwrapUnit(tx2.Send(1))
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
}

func TestMpsc_smoke_threads(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	mpscSpawn(func() {
		defer tx.Drop()
		rs.UnwrapUnit(tx.Send(1))
	})
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
}

func TestMpsc_smoke_port_gone(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rx.Drop()
	assert(t, tx.Send(1) != nil)
}

func TestMpsc_smoke_shared_port_gone(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rx.Drop()
	assert(t, tx.Send(1) != nil)
}

func TestMpsc_smoke_shared_port_gone2(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rx.Drop()
	tx2 := tx.Clone()
	tx.Drop()
	assert(t, tx2.Send(1) != nil)
}

func TestMpsc_port_gone_concurrent(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	mpscSpawn(func() {
		defer rx.Drop()
		rs.Unwrap(rx.Recv())
	})
	for tx.Send(1) == nil {
	}
}

func TestMpsc_port_gone_concurrent_shared(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	tx2 := tx.Clone()
	mpscSpawn(func() {
		defer rx.Drop()
		rs.Unwrap(rx.Recv())
	})
	for tx.Send(1) == nil && tx2.Send(1) == nil {
	}
}

func TestMpsc_smoke_chan_gone(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	tx.Drop()
	_, err := rx.Recv()
	assert(t, err != nil)
}

func TestMpsc_smoke_chan_gone_shared(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	tx2 := tx.Clone()
	tx.Drop()
	tx2.Drop()
	_, err := rx.Recv()
	assert(t, err != nil)
}

func TestMpsc_chan_gone_concurrent(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	mpscSpawn(func() {
		defer tx.Drop()
		rs.UnwrapUnit(tx.Send(1))
		rs.UnwrapUnit(tx.Send(1))
	})
	for {
		if _, err := rx.Recv(); err != nil {
			break
		}
	}
}

func TestMpsc_stress(t *testing.T) {
	count := 10000
	tx, rx := MpscChannel[int32]()
	handle := mpscSpawn(func() {
		defer tx.Drop()
		for range count {
			rs.UnwrapUnit(tx.Send(1))
		}
	})
	for range count {
		assertEq(t, rs.Unwrap(rx.Recv()), 1)
	}
	_, caught := handle.Join()
	assert(t, caught == nil, "thread panicked")
}

func TestMpsc_stress_shared(t *testing.T) {
	const amt = 10000
	const nthreads = 8
	tx, rx := MpscChannel[int32]()

	handle := mpscSpawn(func() {
		defer rx.Drop()
		for range amt * nthreads {
			if rs.Unwrap(rx.Recv()) != 1 {
				panic("assertion `left == right` failed")
			}
		}
		if _, err := rx.TryRecv(); err == nil {
			panic("explicit panic")
		}
	})

	for range nthreads {
		tx := tx.Clone()
		mpscSpawn(func() {
			defer tx.Drop()
			for range amt {
				rs.UnwrapUnit(tx.Send(1))
			}
		})
	}
	tx.Drop()
	_, caught := handle.Join()
	assert(t, caught == nil, "thread panicked")
}

func TestMpsc_send_from_outside_runtime(t *testing.T) {
	tx1, rx1 := MpscChannel[rs.Unit]()
	tx2, rx2 := MpscChannel[int32]()
	t1 := mpscSpawn(func() {
		defer tx1.Drop()
		defer rx2.Drop()
		rs.UnwrapUnit(tx1.Send(rs.Unit{}))
		for range 40 {
			if rs.Unwrap(rx2.Recv()) != 1 {
				panic("assertion `left == right` failed")
			}
		}
	})
	rs.Unwrap(rx1.Recv())
	t2 := mpscSpawn(func() {
		defer tx2.Drop()
		for range 40 {
			rs.UnwrapUnit(tx2.Send(1))
		}
	})
	_, caught := t1.Join()
	assert(t, caught == nil, "thread panicked")
	_, caught = t2.Join()
	assert(t, caught == nil, "thread panicked")
}

func TestMpsc_recv_from_outside_runtime(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	handle := mpscSpawn(func() {
		defer rx.Drop()
		for range 40 {
			if rs.Unwrap(rx.Recv()) != 1 {
				panic("assertion `left == right` failed")
			}
		}
	})
	for range 40 {
		rs.UnwrapUnit(tx.Send(1))
	}
	_, caught := handle.Join()
	assert(t, caught == nil, "thread panicked")
}

func TestMpsc_no_runtime(t *testing.T) {
	tx1, rx1 := MpscChannel[int32]()
	tx2, rx2 := MpscChannel[int32]()
	t1 := mpscSpawn(func() {
		defer rx1.Drop()
		defer tx2.Drop()
		if rs.Unwrap(rx1.Recv()) != 1 {
			panic("assertion `left == right` failed")
		}
		rs.UnwrapUnit(tx2.Send(2))
	})
	t2 := mpscSpawn(func() {
		defer tx1.Drop()
		defer rx2.Drop()
		rs.UnwrapUnit(tx1.Send(1))
		if rs.Unwrap(rx2.Recv()) != 2 {
			panic("assertion `left == right` failed")
		}
	})
	_, caught := t1.Join()
	assert(t, caught == nil, "thread panicked")
	_, caught = t2.Join()
	assert(t, caught == nil, "thread panicked")
}

func TestMpsc_oneshot_single_thread_close_port_first(t *testing.T) {
	// Simple test of closing without sending
	tx, rx := MpscChannel[int32]()
	defer tx.Drop()
	rx.Drop()
}

func TestMpsc_oneshot_single_thread_close_chan_first(t *testing.T) {
	// Simple test of closing without sending
	tx, rx := MpscChannel[int32]()
	defer rx.Drop()
	tx.Drop()
}

func TestMpsc_oneshot_single_thread_send_port_close(t *testing.T) {
	// Testing that the sender cleans up the payload if receiver is closed
	tx, rx := MpscChannel[*int32]()
	rx.Drop()
	assert(t, tx.Send(new(int32)) != nil)
}

func TestMpsc_oneshot_single_thread_recv_chan_close(t *testing.T) {
	quietPanics(t)
	// Receiving on a closed chan will panic
	_, res := mpscSpawn(func() {
		tx, rx := MpscChannel[int32]()
		tx.Drop()
		rs.Unwrap(rx.Recv())
	}).Join()
	// What is our res?
	assert(t, res != nil)
}

func TestMpsc_oneshot_single_thread_send_then_recv(t *testing.T) {
	tx, rx := MpscChannel[*int32]()
	ten := int32(10)
	rs.UnwrapUnit(tx.Send(&ten))
	assert(t, *rs.Unwrap(rx.Recv()) == 10)
}

func TestMpsc_oneshot_single_thread_try_send_open(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	assert(t, tx.Send(10) == nil)
	assert(t, rs.Unwrap(rx.Recv()) == 10)
}

func TestMpsc_oneshot_single_thread_try_send_closed(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rx.Drop()
	assert(t, tx.Send(10) != nil)
}

func TestMpsc_oneshot_single_thread_try_recv_open(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rs.UnwrapUnit(tx.Send(10))
	value, err := rx.Recv()
	assert(t, err == nil && value == 10)
}

func TestMpsc_oneshot_single_thread_try_recv_closed(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	tx.Drop()
	_, err := rx.Recv()
	assert(t, err != nil)
}

func TestMpsc_oneshot_single_thread_peek_data(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	_, err := rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Empty))
	rs.UnwrapUnit(tx.Send(10))
	value, err := rx.TryRecv()
	assertEq(t, err, nil)
	assertEq(t, value, 10)
}

func TestMpsc_oneshot_single_thread_peek_close(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	tx.Drop()
	_, err := rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Disconnected))
	_, err = rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Disconnected))
}

func TestMpsc_oneshot_single_thread_peek_open(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	defer tx.Drop()
	_, err := rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Empty))
}

func TestMpsc_oneshot_multi_task_recv_then_send(t *testing.T) {
	tx, rx := MpscChannel[*int32]()
	mpscSpawn(func() {
		defer rx.Drop()
		if *rs.Unwrap(rx.Recv()) != 10 {
			panic("assertion failed: *rx.recv().unwrap() == 10")
		}
	})

	ten := int32(10)
	rs.UnwrapUnit(tx.Send(&ten))
}

func TestMpsc_oneshot_multi_task_recv_then_close(t *testing.T) {
	quietPanics(t)
	tx, rx := MpscChannel[*int32]()
	mpscSpawn(func() {
		tx.Drop()
	})
	_, res := mpscSpawn(func() {
		defer rx.Drop()
		if *rs.Unwrap(rx.Recv()) != 10 {
			panic("assertion failed: *rx.recv().unwrap() == 10")
		}
	}).Join()
	assert(t, res != nil)
}

func TestMpsc_oneshot_multi_thread_close_stress(t *testing.T) {
	for range mpscStressFactor() {
		tx, rx := MpscChannel[int32]()
		mpscSpawn(func() {
			rx.Drop()
		})
		tx.Drop()
	}
}

func TestMpsc_oneshot_multi_thread_send_close_stress(t *testing.T) {
	quietPanics(t)
	for range mpscStressFactor() {
		tx, rx := MpscChannel[int32]()
		mpscSpawn(func() {
			rx.Drop()
		})
		mpscSpawn(func() {
			defer tx.Drop()
			rs.UnwrapUnit(tx.Send(1))
		}).Join()
	}
}

func TestMpsc_oneshot_multi_thread_recv_close_stress(t *testing.T) {
	quietPanics(t)
	for range mpscStressFactor() {
		tx, rx := MpscChannel[int32]()
		mpscSpawn(func() {
			_, res := mpscSpawn(func() {
				defer rx.Drop()
				rs.Unwrap(rx.Recv())
			}).Join()
			if res == nil {
				panic("assertion failed: res.is_err()")
			}
		})
		mpscSpawn(func() {
			mpscSpawn(func() {
				tx.Drop()
			})
		})
	}
}

func TestMpsc_oneshot_multi_thread_send_recv_stress(t *testing.T) {
	for range mpscStressFactor() {
		tx, rx := MpscChannel[*int]()
		mpscSpawn(func() {
			defer tx.Drop()
			ten := 10
			rs.UnwrapUnit(tx.Send(&ten))
		})
		assert(t, *rs.Unwrap(rx.Recv()) == 10)
	}
}

func mpscStreamSend(tx MpscSender[*int32], i int32, s *rsthread.ScopeHandle) {
	if i == 10 {
		tx.Drop()
		return
	}

	rsthread.ScopeSpawn(s, func() rs.Unit {
		value := i
		rs.UnwrapUnit(tx.Send(&value))
		mpscStreamSend(tx, i+1, s)
		return rs.Unit{}
	})
}

func mpscStreamRecv(rx MpscReceiver[*int32], i int32, s *rsthread.ScopeHandle) {
	if i == 10 {
		rx.Drop()
		return
	}

	rsthread.ScopeSpawn(s, func() rs.Unit {
		if *rs.Unwrap(rx.Recv()) != i {
			panic("assertion failed: *rx.recv().unwrap() == i")
		}
		mpscStreamRecv(rx, i+1, s)
		return rs.Unit{}
	})
}

func TestMpsc_stream_send_recv_stress(t *testing.T) {
	rsthread.Scope(func(s *rsthread.ScopeHandle) rs.Unit {
		for range mpscStressFactor() {
			tx, rx := MpscChannel[*int32]()

			mpscStreamSend(tx, 0, s)
			mpscStreamRecv(rx, 0, s)
		}
		return rs.Unit{}
	})
}

func TestMpsc_oneshot_single_thread_recv_timeout(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	rs.UnwrapUnit(tx.Send(rs.Unit{}))
	_, err := rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, nil)
	_, err = rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, error(MpscRecvTimeoutError_Timeout))
	rs.UnwrapUnit(tx.Send(rs.Unit{}))
	_, err = rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, nil)
}

func TestMpsc_stress_recv_timeout_two_threads(t *testing.T) {
	tx, rx := MpscChannel[int]()
	stress := mpscStressFactor() + 50
	timeout := rstime.DurationFromMillis(5)

	mpscSpawn(func() {
		defer tx.Drop()
		for i := range stress {
			if i%2 == 0 {
				rsthread.Sleep(rstime.DurationMul(timeout, 2))
			}
			rs.UnwrapUnit(tx.Send(1))
		}
	})

	recvCount := 0
	for {
		n, err := rx.RecvTimeout(timeout)
		if err == nil {
			assertEq(t, n, 1)
			recvCount++
			continue
		}
		if err == MpscRecvTimeoutError_Timeout {
			continue
		}
		if err == MpscRecvTimeoutError_Disconnected {
			break
		}
		t.Fatalf("unexpected error %v", err)
	}

	assertEq(t, recvCount, stress)
}

func TestMpsc_recv_timeout_upgrade(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	defer tx.Drop()
	timeout := rstime.DurationFromMillis(1)
	txClone := tx.Clone()
	defer txClone.Drop()

	start := rstime.InstantNow()
	_, err := rx.RecvTimeout(timeout)
	assertEq(t, err, error(MpscRecvTimeoutError_Timeout))
	assert(t, !rstime.InstantNow().Before(start.Add(timeout)))
}

func TestMpsc_stress_recv_timeout_shared(t *testing.T) {
	tx, rx := MpscChannel[int]()
	stress := mpscStressFactor() + 100

	for i := range stress {
		tx := tx.Clone()
		mpscSpawn(func() {
			defer tx.Drop()
			rsthread.Sleep(rstime.DurationFromMillis(uint64(i) * 10))
			rs.UnwrapUnit(tx.Send(1))
		})
	}

	tx.Drop()

	recvCount := 0
	for {
		n, err := rx.RecvTimeout(rstime.DurationFromMillis(10))
		if err == nil {
			assertEq(t, n, 1)
			recvCount++
			continue
		}
		if err == MpscRecvTimeoutError_Timeout {
			continue
		}
		if err == MpscRecvTimeoutError_Disconnected {
			break
		}
		t.Fatalf("unexpected error %v", err)
	}

	assertEq(t, recvCount, stress)
}

func TestMpsc_very_long_recv_timeout_wont_panic(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	joinHandle := rsthread.Spawn(func() error {
		defer rx.Drop()
		_, err := rx.RecvTimeout(rstime.DurationFromSecs(math.MaxUint64))
		return err
	})
	rsthread.Sleep(rstime.DurationFromSecs(1))
	assert(t, tx.Send(rs.Unit{}) == nil)
	assertEq(t, joinHandle.JoinUnwrap(), nil)
}

func TestMpsc_recv_a_lot(t *testing.T) {
	count := 10000
	// Regression test that we don't run out of stack in scheduler context
	tx, rx := MpscChannel[rs.Unit]()
	for range count {
		rs.UnwrapUnit(tx.Send(rs.Unit{}))
	}
	for range count {
		rs.Unwrap(rx.Recv())
	}
}

func TestMpsc_shared_recv_timeout(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	total := 5
	for range total {
		tx := tx.Clone()
		mpscSpawn(func() {
			defer tx.Drop()
			rs.UnwrapUnit(tx.Send(rs.Unit{}))
		})
	}

	for range total {
		rs.Unwrap(rx.Recv())
	}

	_, err := rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, error(MpscRecvTimeoutError_Timeout))
	rs.UnwrapUnit(tx.Send(rs.Unit{}))
	_, err = rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, nil)
}

func TestMpsc_shared_chan_stress(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	total := mpscStressFactor() + 100
	for range total {
		tx := tx.Clone()
		mpscSpawn(func() {
			defer tx.Drop()
			rs.UnwrapUnit(tx.Send(rs.Unit{}))
		})
	}

	for range total {
		rs.Unwrap(rx.Recv())
	}
}

func TestMpsc_test_nested_recv_iter(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	totalTx, totalRx := MpscChannel[int32]()

	mpscSpawn(func() {
		defer rx.Drop()
		defer totalTx.Drop()
		acc := int32(0)
		for x := range rx.Iter() {
			acc += x
		}
		rs.UnwrapUnit(totalTx.Send(acc))
	})

	rs.UnwrapUnit(tx.Send(3))
	rs.UnwrapUnit(tx.Send(1))
	rs.UnwrapUnit(tx.Send(2))
	tx.Drop()
	assertEq(t, rs.Unwrap(totalRx.Recv()), 6)
}

func TestMpsc_test_recv_iter_break(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	countTx, countRx := MpscChannel[int32]()

	mpscSpawn(func() {
		defer rx.Drop()
		defer countTx.Drop()
		count := int32(0)
		for x := range rx.Iter() {
			if count >= 3 {
				break
			} else {
				count += x
			}
		}
		rs.UnwrapUnit(countTx.Send(count))
	})

	rs.UnwrapUnit(tx.Send(2))
	rs.UnwrapUnit(tx.Send(2))
	rs.UnwrapUnit(tx.Send(2))
	_ = tx.Send(2)
	tx.Drop()
	assertEq(t, rs.Unwrap(countRx.Recv()), 4)
}

func TestMpsc_test_recv_try_iter(t *testing.T) {
	requestTx, requestRx := MpscChannel[rs.Unit]()
	responseTx, responseRx := MpscChannel[int]()

	// Request `x`s until we have `6`.
	thread := rsthread.Spawn(func() int {
		defer requestTx.Drop()
		defer responseRx.Drop()
		count := 0
		for {
			for x := range responseRx.TryIter() {
				count += x
				if count == 6 {
					return count
				}
			}
			rs.UnwrapUnit(requestTx.Send(rs.Unit{}))
		}
	})

	for range requestRx.Iter() {
		if responseTx.Send(2) != nil {
			break
		}
	}

	assertEq(t, thread.JoinUnwrap(), 6)
}

func TestMpsc_test_recv_into_iter_owned(t *testing.T) {
	next, stop := func() (func() (int32, bool), func()) {
		tx, rx := MpscChannel[int32]()
		defer tx.Drop()
		rs.UnwrapUnit(tx.Send(1))
		rs.UnwrapUnit(tx.Send(2))

		return iter.Pull(rx.IntoIter())
	}()
	defer stop()
	first, ok := next()
	assert(t, ok)
	assertEq(t, first, 1)
	second, ok := next()
	assert(t, ok)
	assertEq(t, second, 2)
	_, ok = next()
	assertEq(t, ok, false)
}

func TestMpsc_test_recv_into_iter_borrowed(t *testing.T) {
	tx, rx := MpscChannel[int32]()
	rs.UnwrapUnit(tx.Send(1))
	rs.UnwrapUnit(tx.Send(2))
	tx.Drop()
	next, stop := iter.Pull(rx.Iter())
	defer stop()
	first, ok := next()
	assert(t, ok)
	assertEq(t, first, 1)
	second, ok := next()
	assert(t, ok)
	assertEq(t, second, 2)
	_, ok = next()
	assertEq(t, ok, false)
}

func TestMpsc_try_recv_states(t *testing.T) {
	tx1, rx1 := MpscChannel[int32]()
	tx2, rx2 := MpscChannel[rs.Unit]()
	tx3, rx3 := MpscChannel[rs.Unit]()
	mpscSpawn(func() {
		defer rx2.Drop()
		defer tx3.Drop()
		rs.Unwrap(rx2.Recv())
		rs.UnwrapUnit(tx1.Send(1))
		rs.UnwrapUnit(tx3.Send(rs.Unit{}))
		rs.Unwrap(rx2.Recv())
		tx1.Drop()
		rs.UnwrapUnit(tx3.Send(rs.Unit{}))
	})

	_, err := rx1.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Empty))
	rs.UnwrapUnit(tx2.Send(rs.Unit{}))
	rs.Unwrap(rx3.Recv())
	value, err := rx1.TryRecv()
	assertEq(t, err, nil)
	assertEq(t, value, 1)
	_, err = rx1.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Empty))
	rs.UnwrapUnit(tx2.Send(rs.Unit{}))
	rs.Unwrap(rx3.Recv())
	_, err = rx1.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Disconnected))
}

// This bug used to end up in a livelock inside of the Receiver destructor
// because the internal state of the Shared packet was corrupted
func TestMpsc_destroy_upgraded_shared_port_when_sender_still_active(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	tx2, rx2 := MpscChannel[rs.Unit]()
	mpscSpawn(func() {
		defer tx2.Drop()
		rs.Unwrap(rx.Recv()) // wait on a oneshot
		rx.Drop()            // destroy a shared
		rs.UnwrapUnit(tx2.Send(rs.Unit{}))
	})
	// make sure the other thread has gone to sleep
	for range 5000 {
		rsthread.YieldNow()
	}

	// upgrade to a shared chan and send a message
	t2 := tx.Clone()
	defer t2.Drop()
	tx.Drop()
	rs.UnwrapUnit(t2.Send(rs.Unit{}))

	// wait for the child thread to exit before we exit
	rs.Unwrap(rx2.Recv())
}

func TestMpsc_issue_32114(t *testing.T) {
	tx, rx := MpscChannel[int]()
	rx.Drop()
	_ = tx.Send(123)
	err := tx.Send(123)
	sendError, ok := err.(*MpscSendError[int])
	assert(t, ok, err)
	assertEq(t, sendError.F0, 123)
}

func TestMpsc_issue_39364(t *testing.T) {
	tx, rx := MpscChannel[rs.Unit]()
	thread := rsthread.Spawn(func() MpscSender[rs.Unit] {
		rsthread.Sleep(rstime.DurationFromMillis(300))
		tx.Clone().Drop()
		// Don't drop; hand back to caller.
		return tx
	})

	_, _ = rx.RecvTimeout(rstime.DurationFromMillis(500))
	tx2 := thread.JoinUnwrap() // delay dropping until end of test
	defer tx2.Drop()
	_, _ = rx.RecvTimeout(rstime.DurationFromMillis(500))
}
