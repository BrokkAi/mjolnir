package collections

import (
	"fmt"
	"reflect"
	"testing"
)

// check verifies what the deque promises about its buffer: head inside the
// buffer (zero for an empty buffer), length within the capacity, and every
// slot outside the live range holding the zero value, so that the collector
// does not keep elements alive that Rust has moved out. A nil t turns a
// broken invariant into a panic, which the oracle steps report as a
// difference from Rust.
func (d VecDeque[T]) check(t testing.TB) {
	if t != nil {
		t.Helper()
	}
	if err := d.checkInvariants(); err != nil {
		if t == nil {
			panic("deque invariant broken: " + err.Error())
		}
		t.Fatalf("deque invariant broken: %v", err)
	}
}

func (d VecDeque[T]) checkInvariants() error {
	capacity := len(d.buf)
	if d.length < 0 || d.length > capacity {
		return fmt.Errorf("length %d outside a buffer of %d", d.length, capacity)
	}
	if capacity == 0 {
		if d.head != 0 {
			return fmt.Errorf("head %d in an empty buffer", d.head)
		}
		return nil
	}
	if d.head < 0 || d.head >= capacity {
		return fmt.Errorf("head %d outside a buffer of %d", d.head, capacity)
	}
	if vecDequeElementSize[T]() == 0 {
		return nil
	}
	for slot := range d.buf {
		// The logical index of the slot, counted from head.
		logical := slot - d.head
		if logical < 0 {
			logical += capacity
		}
		if logical >= d.length && !reflect.ValueOf(&d.buf[slot]).Elem().IsZero() {
			return fmt.Errorf("slot %d (head %d, length %d, capacity %d) outside the live range holds %v",
				slot, d.head, d.length, capacity, d.buf[slot])
		}
	}
	return nil
}

// cloneExact is VecDeque::clone as Rust does it: a buffer of exactly the
// length, filled as a TrustedLen iterator fills it. rs.Clone, which product
// code calls, gives the same elements in a buffer grown by pushes; the oracle
// scripts compare the capacity after a clone, so they clone this way.
func (d VecDeque[T]) cloneExact(clone func(T) T) VecDeque[T] {
	deq := VecDequeWithCapacity[T](d.length)
	a, b := d.AsSlices()
	index := 0
	deq.extendTrustedLen(d.length, func() T {
		var element T
		if index < len(a) {
			element = a[index]
		} else {
			element = b[index-len(a)]
		}
		index++
		return clone(element)
	})
	return deq
}
