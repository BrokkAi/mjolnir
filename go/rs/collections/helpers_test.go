package collections

// Helpers shared by the tests ported from the Rust standard library: the
// assertions in Rust's terms, and the types of alloctests/testing
// (ord_chaos.rs, rng.rs) that the B-tree suites build their keys from.

import (
	"iter"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// expectEq is assert_eq!: the two values are equal by rs.Eq.
func expectEq[T any](t testing.TB, got, want T, what ...any) {
	t.Helper()
	if !rs.Eq(got, want) {
		t.Fatalf("assertion `left == right` failed%s\n  left: %s\n right: %s", describe(what), rs.DebugString(got), rs.DebugString(want))
	}
}

// expectNe is assert_ne!.
func expectNe[T any](t testing.TB, got, want T, what ...any) {
	t.Helper()
	if rs.Eq(got, want) {
		t.Fatalf("assertion `left != right` failed%s\n  left: %s\n right: %s", describe(what), rs.DebugString(got), rs.DebugString(want))
	}
}

// expect is assert!.
func expect(t testing.TB, cond bool, what ...any) {
	t.Helper()
	if !cond {
		t.Fatalf("assertion failed%s", describe(what))
	}
}

func describe(what []any) string {
	if len(what) == 0 {
		return ""
	}
	return ": " + rs.Format(what[0].(string), what[1:]...)
}

// panicMessage runs f and returns the message it panicked with, and whether
// it panicked.
func panicMessage(f func()) (message string, panicked bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			message, panicked = rs.PanicInfoOf(recovered).Message, true
		}
	}()
	f()
	return "", false
}

// shouldPanic is #[should_panic(expected = "..")]: f panics with a message
// that contains expected (any message when expected is empty).
func shouldPanic(t testing.TB, expected string, f func()) {
	t.Helper()
	message, panicked := panicMessage(f)
	if !panicked {
		t.Fatalf("expected a panic with %q, got none", expected)
	}
	if !strings.Contains(message, expected) {
		t.Fatalf("panic did not contain expected string\n      panic message: %q\n expected substring: %q", message, expected)
	}
}

// collect is `.collect::<Vec<_>>()` of a sequence. It is never nil.
func collect[T any](seq iter.Seq[T]) []T {
	collected := []T{}
	for element := range seq {
		collected = append(collected, element)
	}
	return collected
}

// collectPairs is `.collect::<Vec<(K, V)>>()` of a map's entries, with the
// values read through their pointers.
func collectPairs[K, V any](seq iter.Seq2[K, *V]) []rs.Tuple2[K, V] {
	collected := []rs.Tuple2[K, V]{}
	for key, value := range seq {
		collected = append(collected, rs.T2(key, *value))
	}
	return collected
}

// collectOwned is collectPairs for a sequence that moves its values out.
func collectOwned[K, V any](seq iter.Seq2[K, V]) []rs.Tuple2[K, V] {
	collected := []rs.Tuple2[K, V]{}
	for key, value := range seq {
		collected = append(collected, rs.T2(key, value))
	}
	return collected
}

// count is Iterator::count.
func count[T any](seq iter.Seq[T]) int {
	n := 0
	for range seq {
		n++
	}
	return n
}

// count2 is Iterator::count for a sequence of pairs.
func count2[K, V any](seq iter.Seq2[K, V]) int {
	n := 0
	for range seq {
		n++
	}
	return n
}

// seqMin and seqMax are Iterator::min and Iterator::max: the first of the
// smallest and the last of the greatest elements, by rs.Cmp.
func seqMin[T any](seq iter.Seq[T]) rs.Option[T] {
	var best rs.Option[T]
	for element := range seq {
		if current, ok := best.Get(); !ok || rs.Cmp(element, current) < 0 {
			best = rs.Some(element)
		}
	}
	return best
}

func seqMax[T any](seq iter.Seq[T]) rs.Option[T] {
	var best rs.Option[T]
	for element := range seq {
		if current, ok := best.Get(); !ok || rs.Cmp(element, current) >= 0 {
			best = rs.Some(element)
		}
	}
	return best
}

// pairsOf turns a sequence of a key and a value pointer into pairs, which
// is how `(&k, &v)` items compare in the Rust tests.
func pairsOf[K, V any](seq iter.Seq2[K, *V]) iter.Seq[rs.Tuple2[K, V]] {
	return func(yield func(rs.Tuple2[K, V]) bool) {
		for key, value := range seq {
			if !yield(rs.T2(key, *value)) {
				return
			}
		}
	}
}

// identityPairs is `(lo..hi).map(|i| (i, i))`.
func identityPairs[T ~int | ~int32 | ~uint32 | ~int64 | ~uint64 | ~uint8 | ~uint16](lo, hi T) iter.Seq2[T, T] {
	return func(yield func(T, T) bool) {
		for i := lo; i < hi; i++ {
			if !yield(i, i) {
				return
			}
		}
	}
}

// intRange is `lo..hi` collected.
func intRange[T ~int | ~int32 | ~uint32 | ~int64](lo, hi T) []T {
	values := []T{}
	for i := lo; i < hi; i++ {
		values = append(values, i)
	}
	return values
}

// deterministicRng is DeterministicRng in alloctests/testing/rng.rs, a
// XorShiftRng.
type deterministicRng struct {
	count      int
	x, y, z, w uint32
}

func newDeterministicRng() *deterministicRng {
	return &deterministicRng{count: 0, x: 0x193a6754, y: 0xa8a7d469, z: 0x97830e05, w: 0x113ba7bb}
}

// next guarantees that each returned number is unique.
func (r *deterministicRng) next() uint32 {
	r.count++
	if r.count > 70029 {
		panic("assertion failed: self.count <= 70029")
	}
	x := r.x
	t := x ^ (x << 11)
	r.x = r.y
	r.y = r.z
	r.z = r.w
	w := r.w
	r.w = w ^ (w >> 19) ^ (t ^ (t >> 8))
	return r.w
}

// cyclic3 is Cyclic3 in ord_chaos.rs: a minimal type with an Ord
// implementation violating transitivity.
type cyclic3 uint8

const (
	cyclicA cyclic3 = iota
	cyclicB
	cyclicC
)

func (c cyclic3) Compare(other cyclic3) int {
	switch {
	case c == other:
		return 0
	case c == cyclicA && other == cyclicB, c == cyclicB && other == cyclicC, c == cyclicC && other == cyclicA:
		return -1
	default:
		return 1
	}
}

func (c cyclic3) Equal(other cyclic3) bool { return c.Compare(other) == 0 }

// governor controls the ordering of values wrapped by governed.
type governor struct{ flipped bool }

func (g *governor) flip() { g.flipped = !g.flipped }

// governed is Governed in ord_chaos.rs: a type with an Ord implementation
// that forms a total order at any moment, but can suddenly be made to
// invert that total order.
type governed struct {
	value int
	gov   *governor
}

func (g governed) Compare(other governed) int {
	if g.gov != other.gov {
		panic("assertion failed: ptr::eq(self.1, other.1)")
	}
	order := rs.Cmp(g.value, other.value)
	if g.gov.flipped {
		return -order
	}
	return order
}

func (g governed) Equal(other governed) bool {
	if g.gov != other.gov {
		panic("assertion failed: ptr::eq(self.1, other.1)")
	}
	return g.value == other.value
}

// idBased is IdBased in ord_chaos.rs: comparison based only on the ID, the
// name is ignored.
type idBased struct {
	id   uint32
	name string
}

func (i idBased) Compare(other idBased) int { return rs.Cmp(i.id, other.id) }

func (i idBased) Equal(other idBased) bool { return i.id == other.id }

// crashTestDummy is the part of CrashTestDummy in crash_test.rs that Go can
// observe: an id that orders and identifies its instances, and a count of
// the queries made of them. (Rust's dummies also count clones and drops.)
type crashTestDummy struct {
	id      int
	queried int
}

// dummyPanic is crash_test::Panic, for the events Go has.
type dummyPanic uint8

const (
	panicNever dummyPanic = iota
	panicInQuery
)

// dummyInstance is crash_test::Instance.
type dummyInstance struct {
	origin *crashTestDummy
	panic  dummyPanic
}

func (d *crashTestDummy) spawn(p dummyPanic) dummyInstance {
	return dummyInstance{origin: d, panic: p}
}

func (i dummyInstance) id() int { return i.origin.id }

// query is some anonymous query, the result of which is already given.
func (i dummyInstance) query(result bool) bool {
	i.origin.queried++
	if i.panic == panicInQuery {
		panic("panic in `query`")
	}
	return result
}

func (i dummyInstance) Compare(other dummyInstance) int { return rs.Cmp(i.id(), other.id()) }

func (i dummyInstance) Equal(other dummyInstance) bool { return i.id() == other.id() }

// sorted returns a sorted copy, as `data.sort()` leaves data.
func sorted[T any](values []T) []T {
	copied := slices.Clone(values)
	slices.SortFunc(copied, rs.Cmp[T])
	return copied
}
