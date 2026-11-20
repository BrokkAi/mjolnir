// Port of std 1.99.1 tests/sync/mpsc_sync.rs
//
// Not ported:
//   - send_timeout: SyncSender::send_timeout is hidden and unstable
//     (std_internals), and its error type is the unstable mpmc one.

package rssync

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsthread"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

func TestMpscSync_smoke(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	rs.UnwrapUnit(tx.Send(1))
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
}

func TestMpscSync_drop_full(t *testing.T) {
	tx, rx := MpscSyncChannel[*int](1)
	defer rx.Drop()
	rs.UnwrapUnit(tx.Send(new(int)))
}

func TestMpscSync_smoke_shared(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	rs.UnwrapUnit(tx.Send(1))
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
	tx2 := tx.Clone()
	rs.UnwrapUnit(tx2.Send(1))
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
}

func TestMpscSync_recv_timeout(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	_, err := rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, error(MpscRecvTimeoutError_Timeout))
	rs.UnwrapUnit(tx.Send(1))
	value, err := rx.RecvTimeout(rstime.DurationFromMillis(1))
	assertEq(t, err, nil)
	assertEq(t, value, 1)
}

func TestMpscSync_smoke_threads(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	mpscSpawn(func() {
		defer tx.Drop()
		rs.UnwrapUnit(tx.Send(1))
	})
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
}

func TestMpscSync_smoke_port_gone(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	rx.Drop()
	assert(t, tx.Send(1) != nil)
}

func TestMpscSync_smoke_shared_port_gone2(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	rx.Drop()
	tx2 := tx.Clone()
	tx.Drop()
	assert(t, tx2.Send(1) != nil)
}

func TestMpscSync_port_gone_concurrent(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	mpscSpawn(func() {
		defer rx.Drop()
		rs.Unwrap(rx.Recv())
	})
	for tx.Send(1) == nil {
	}
}

func TestMpscSync_port_gone_concurrent_shared(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	tx2 := tx.Clone()
	mpscSpawn(func() {
		defer rx.Drop()
		rs.Unwrap(rx.Recv())
	})
	for tx.Send(1) == nil && tx2.Send(1) == nil {
	}
}

func TestMpscSync_smoke_chan_gone(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	tx.Drop()
	_, err := rx.Recv()
	assert(t, err != nil)
}

func TestMpscSync_smoke_chan_gone_shared(t *testing.T) {
	tx, rx := MpscSyncChannel[rs.Unit](0)
	tx2 := tx.Clone()
	tx.Drop()
	tx2.Drop()
	_, err := rx.Recv()
	assert(t, err != nil)
}

func TestMpscSync_chan_gone_concurrent(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
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

func TestMpscSync_stress(t *testing.T) {
	count := 10000
	tx, rx := MpscSyncChannel[int32](0)
	mpscSpawn(func() {
		defer tx.Drop()
		for range count {
			rs.UnwrapUnit(tx.Send(1))
		}
	})
	for range count {
		assertEq(t, rs.Unwrap(rx.Recv()), 1)
	}
}

func TestMpscSync_stress_recv_timeout_two_threads(t *testing.T) {
	count := 10000
	tx, rx := MpscSyncChannel[int32](0)

	mpscSpawn(func() {
		defer tx.Drop()
		for range count {
			rs.UnwrapUnit(tx.Send(1))
		}
	})

	recvCount := 0
	for {
		v, err := rx.RecvTimeout(rstime.DurationFromMillis(1))
		if err == nil {
			assertEq(t, v, 1)
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

	assertEq(t, recvCount, count)
}

func TestMpscSync_stress_recv_timeout_shared(t *testing.T) {
	const amt = 1000
	const nthreads = 8
	tx, rx := MpscSyncChannel[int32](0)
	dtx, drx := MpscSyncChannel[rs.Unit](0)

	mpscSpawn(func() {
		defer rx.Drop()
		defer dtx.Drop()
		recvCount := 0
		for {
			v, err := rx.RecvTimeout(rstime.DurationFromMillis(10))
			if err == nil {
				if v != 1 {
					panic("assertion `left == right` failed")
				}
				recvCount++
				continue
			}
			if err == MpscRecvTimeoutError_Timeout {
				continue
			}
			if err == MpscRecvTimeoutError_Disconnected {
				break
			}
			panic(err)
		}

		if recvCount != amt*nthreads {
			panic("assertion `left == right` failed")
		}
		if _, err := rx.TryRecv(); err == nil {
			panic("assertion failed: rx.try_recv().is_err()")
		}

		rs.UnwrapUnit(dtx.Send(rs.Unit{}))
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

	rs.Unwrap(drx.Recv())
}

func TestMpscSync_stress_shared(t *testing.T) {
	const amt = 1000
	const nthreads = 8
	tx, rx := MpscSyncChannel[int32](0)
	dtx, drx := MpscSyncChannel[rs.Unit](0)

	mpscSpawn(func() {
		defer rx.Drop()
		defer dtx.Drop()
		for range amt * nthreads {
			if rs.Unwrap(rx.Recv()) != 1 {
				panic("assertion `left == right` failed")
			}
		}
		if _, err := rx.TryRecv(); err == nil {
			panic("explicit panic")
		}
		rs.UnwrapUnit(dtx.Send(rs.Unit{}))
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
	rs.Unwrap(drx.Recv())
}

func TestMpscSync_oneshot_single_thread_close_port_first(t *testing.T) {
	// Simple test of closing without sending
	tx, rx := MpscSyncChannel[int32](0)
	defer tx.Drop()
	rx.Drop()
}

func TestMpscSync_oneshot_single_thread_close_chan_first(t *testing.T) {
	// Simple test of closing without sending
	tx, rx := MpscSyncChannel[int32](0)
	defer rx.Drop()
	tx.Drop()
}

func TestMpscSync_oneshot_single_thread_send_port_close(t *testing.T) {
	// Testing that the sender cleans up the payload if receiver is closed
	tx, rx := MpscSyncChannel[*int32](0)
	rx.Drop()
	assert(t, tx.Send(new(int32)) != nil)
}

func TestMpscSync_oneshot_single_thread_recv_chan_close(t *testing.T) {
	quietPanics(t)
	// Receiving on a closed chan will panic
	_, res := mpscSpawn(func() {
		tx, rx := MpscSyncChannel[int32](0)
		tx.Drop()
		rs.Unwrap(rx.Recv())
	}).Join()
	// What is our res?
	assert(t, res != nil)
}

func TestMpscSync_oneshot_single_thread_send_then_recv(t *testing.T) {
	tx, rx := MpscSyncChannel[*int32](1)
	ten := int32(10)
	rs.UnwrapUnit(tx.Send(&ten))
	assert(t, *rs.Unwrap(rx.Recv()) == 10)
}

func TestMpscSync_oneshot_single_thread_try_send_open(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	assertEq(t, tx.TrySend(10), nil)
	assert(t, rs.Unwrap(rx.Recv()) == 10)
}

func TestMpscSync_oneshot_single_thread_try_send_closed(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	rx.Drop()
	err, ok := tx.TrySend(10).(*MpscTrySendError_Disconnected[int32])
	assert(t, ok)
	assertEq(t, err.F0, 10)
}

func TestMpscSync_oneshot_single_thread_try_send_closed2(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	defer rx.Drop()
	err, ok := tx.TrySend(10).(*MpscTrySendError_Full[int32])
	assert(t, ok)
	assertEq(t, err.F0, 10)
}

func TestMpscSync_oneshot_single_thread_try_recv_open(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	rs.UnwrapUnit(tx.Send(10))
	value, err := rx.Recv()
	assert(t, err == nil && value == 10)
}

func TestMpscSync_oneshot_single_thread_try_recv_closed(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	tx.Drop()
	_, err := rx.Recv()
	assert(t, err != nil)
}

func TestMpscSync_oneshot_single_thread_try_recv_closed_with_data(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	rs.UnwrapUnit(tx.Send(10))
	tx.Drop()
	value, err := rx.TryRecv()
	assertEq(t, err, nil)
	assertEq(t, value, 10)
	_, err = rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Disconnected))
}

func TestMpscSync_oneshot_single_thread_peek_data(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	_, err := rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Empty))
	rs.UnwrapUnit(tx.Send(10))
	value, err := rx.TryRecv()
	assertEq(t, err, nil)
	assertEq(t, value, 10)
}

func TestMpscSync_oneshot_single_thread_peek_close(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	tx.Drop()
	_, err := rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Disconnected))
	_, err = rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Disconnected))
}

func TestMpscSync_oneshot_single_thread_peek_open(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	defer tx.Drop()
	_, err := rx.TryRecv()
	assertEq(t, err, error(MpscTryRecvError_Empty))
}

func TestMpscSync_oneshot_multi_task_recv_then_send(t *testing.T) {
	tx, rx := MpscSyncChannel[*int32](0)
	mpscSpawn(func() {
		defer rx.Drop()
		if *rs.Unwrap(rx.Recv()) != 10 {
			panic("assertion failed: *rx.recv().unwrap() == 10")
		}
	})

	ten := int32(10)
	rs.UnwrapUnit(tx.Send(&ten))
}

func TestMpscSync_oneshot_multi_task_recv_then_close(t *testing.T) {
	quietPanics(t)
	tx, rx := MpscSyncChannel[*int32](0)
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

func TestMpscSync_oneshot_multi_thread_close_stress(t *testing.T) {
	for range mpscStressFactor() {
		tx, rx := MpscSyncChannel[int32](0)
		mpscSpawn(func() {
			rx.Drop()
		})
		tx.Drop()
	}
}

func TestMpscSync_oneshot_multi_thread_send_close_stress(t *testing.T) {
	quietPanics(t)
	for range mpscStressFactor() {
		tx, rx := MpscSyncChannel[int32](0)
		mpscSpawn(func() {
			rx.Drop()
		})
		mpscSpawn(func() {
			defer tx.Drop()
			rs.UnwrapUnit(tx.Send(1))
		}).Join()
	}
}

func TestMpscSync_oneshot_multi_thread_recv_close_stress(t *testing.T) {
	quietPanics(t)
	for range mpscStressFactor() {
		tx, rx := MpscSyncChannel[int32](0)
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

func TestMpscSync_oneshot_multi_thread_send_recv_stress(t *testing.T) {
	for range mpscStressFactor() {
		tx, rx := MpscSyncChannel[*int32](0)
		mpscSpawn(func() {
			defer tx.Drop()
			ten := int32(10)
			rs.UnwrapUnit(tx.Send(&ten))
		})
		assert(t, *rs.Unwrap(rx.Recv()) == 10)
	}
}

func mpscSyncStreamSend(tx MpscSyncSender[*int32], i int32) {
	if i == 10 {
		tx.Drop()
		return
	}

	mpscSpawn(func() {
		value := i
		rs.UnwrapUnit(tx.Send(&value))
		mpscSyncStreamSend(tx, i+1)
	})
}

func mpscSyncStreamRecv(rx MpscReceiver[*int32], i int32) {
	if i == 10 {
		rx.Drop()
		return
	}

	mpscSpawn(func() {
		if *rs.Unwrap(rx.Recv()) != i {
			panic("assertion failed: *rx.recv().unwrap() == i")
		}
		mpscSyncStreamRecv(rx, i+1)
	})
}

func TestMpscSync_stream_send_recv_stress(t *testing.T) {
	for range mpscStressFactor() {
		tx, rx := MpscSyncChannel[*int32](0)

		mpscSyncStreamSend(tx, 0)
		mpscSyncStreamRecv(rx, 0)
	}
}

func TestMpscSync_recv_a_lot(t *testing.T) {
	count := 10000
	// Regression test that we don't run out of stack in scheduler context
	tx, rx := MpscSyncChannel[rs.Unit](count)
	for range count {
		rs.UnwrapUnit(tx.Send(rs.Unit{}))
	}
	for range count {
		rs.Unwrap(rx.Recv())
	}
}

func TestMpscSync_shared_chan_stress(t *testing.T) {
	tx, rx := MpscSyncChannel[rs.Unit](0)
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

func TestMpscSync_test_nested_recv_iter(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	totalTx, totalRx := MpscSyncChannel[int32](0)

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

func TestMpscSync_test_recv_iter_break(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	countTx, countRx := MpscSyncChannel[int32](0)

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
	_ = tx.TrySend(2)
	tx.Drop()
	assertEq(t, rs.Unwrap(countRx.Recv()), 4)
}

func TestMpscSync_try_recv_states(t *testing.T) {
	tx1, rx1 := MpscSyncChannel[int32](1)
	tx2, rx2 := MpscSyncChannel[rs.Unit](1)
	tx3, rx3 := MpscSyncChannel[rs.Unit](1)
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
func TestMpscSync_destroy_upgraded_shared_port_when_sender_still_active(t *testing.T) {
	tx, rx := MpscSyncChannel[rs.Unit](0)
	tx2, rx2 := MpscSyncChannel[rs.Unit](0)
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

func TestMpscSync_send1(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	mpscSpawn(func() {
		defer rx.Drop()
		rs.Unwrap(rx.Recv())
	})
	assertEq(t, tx.Send(1), nil)
}

func TestMpscSync_send2(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	mpscSpawn(func() {
		rx.Drop()
	})
	assert(t, tx.Send(1) != nil)
}

func TestMpscSync_send3(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	assertEq(t, tx.Send(1), nil)
	mpscSpawn(func() {
		rx.Drop()
	})
	assert(t, tx.Send(1) != nil)
}

func TestMpscSync_send4(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	tx2 := tx.Clone()
	done, donerx := MpscChannel[rs.Unit]()
	done2 := done.Clone()
	mpscSpawn(func() {
		defer tx.Drop()
		defer done.Drop()
		if tx.Send(1) == nil {
			panic("assertion failed: tx.send(1).is_err()")
		}
		rs.UnwrapUnit(done.Send(rs.Unit{}))
	})
	mpscSpawn(func() {
		defer tx2.Drop()
		defer done2.Drop()
		if tx2.Send(2) == nil {
			panic("assertion failed: tx2.send(2).is_err()")
		}
		rs.UnwrapUnit(done2.Send(rs.Unit{}))
	})
	rx.Drop()
	rs.Unwrap(donerx.Recv())
	rs.Unwrap(donerx.Recv())
}

func TestMpscSync_try_send1(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	defer rx.Drop()
	err, ok := tx.TrySend(1).(*MpscTrySendError_Full[int32])
	assert(t, ok)
	assertEq(t, err.F0, 1)
}

func TestMpscSync_try_send2(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	defer rx.Drop()
	assertEq(t, tx.TrySend(1), nil)
	err, ok := tx.TrySend(1).(*MpscTrySendError_Full[int32])
	assert(t, ok)
	assertEq(t, err.F0, 1)
}

func TestMpscSync_try_send3(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](1)
	assertEq(t, tx.TrySend(1), nil)
	rx.Drop()
	err, ok := tx.TrySend(1).(*MpscTrySendError_Disconnected[int32])
	assert(t, ok)
	assertEq(t, err.F0, 1)
}

func TestMpscSync_issue_15761(t *testing.T) {
	repro := func() {
		tx1, rx1 := MpscSyncChannel[rs.Unit](3)
		tx2, rx2 := MpscSyncChannel[rs.Unit](3)

		mpscSpawn(func() {
			defer rx1.Drop()
			defer tx2.Drop()
			rs.Unwrap(rx1.Recv())
			rs.UnwrapUnit(tx2.TrySend(rs.Unit{}))
		})

		rs.UnwrapUnit(tx1.TrySend(rs.Unit{}))
		rs.Unwrap(rx2.Recv())
	}

	for range 100 {
		repro()
	}
}

func TestMpscSync_drop_unreceived(t *testing.T) {
	tx, rx := MpscSyncChannel[rs.Arc[rs.Unit]](1)
	msg := rs.ArcNew(rs.Unit{})
	weak := rs.ArcDowngrade(msg)
	assert(t, tx.Send(msg) == nil)
	rx.Drop()
	// Messages should be dropped immediately when the last receiver is destroyed.
	assert(t, weak.Upgrade().IsNone())
	tx.Drop()
}

// What the port adds to Rust's tests.

func TestMpscSync_port_a_rendezvous_send_waits_for_the_receiver(t *testing.T) {
	tx, rx := MpscSyncChannel[int32](0)
	var sent AtomicBool
	sender := mpscSpawn(func() {
		defer tx.Drop()
		rs.UnwrapUnit(tx.Send(1))
		sent.Store(true, Ordering_SeqCst)
	})
	rsthread.Sleep(rstime.DurationFromMillis(30))
	assert(t, !sent.Load(Ordering_SeqCst), "the send returned before anyone received")
	assertEq(t, rs.Unwrap(rx.Recv()), 1)
	sender.JoinUnwrap()
	assert(t, sent.Load(Ordering_SeqCst))
}

func TestMpscSync_port_a_full_channel_wakes_every_blocked_sender(t *testing.T) {
	// Two receives in a row must wake two senders blocked on a full
	// channel, not the same one twice.
	tx, rx := MpscSyncChannel[int32](2)
	rs.UnwrapUnit(tx.Send(0))
	rs.UnwrapUnit(tx.Send(0))
	senders := make([]*rsthread.JoinHandle[rs.Unit], 2)
	for i := range senders {
		tx := tx.Clone()
		senders[i] = mpscSpawn(func() {
			defer tx.Drop()
			rs.UnwrapUnit(tx.Send(int32(i + 1)))
		})
	}
	tx.Drop()
	rsthread.Sleep(rstime.DurationFromMillis(20))
	rs.Unwrap(rx.Recv())
	rs.Unwrap(rx.Recv())
	for _, sender := range senders {
		sender.JoinUnwrap()
	}
	sum := int32(0)
	for value := range rx.Iter() {
		sum += value
	}
	assertEq(t, sum, 3)
}
