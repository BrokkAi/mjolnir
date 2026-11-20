// Port of core 1.99.1 src/iter/traits/iterator.rs, src/iter/traits/accum.rs
// and src/iter/sources/repeat_n.rs (Iterator's methods, over slices)

package rs

import (
	"cmp"
	"math"
)

// Iterator methods over slices.
//
// Rust's iterator chains are lazy. In this port an iterator over a slice is
// the slice: `v.iter().map(f).collect()` is rs.Map(v, f), and an adapter
// returns a new slice holding what the Rust iterator would yield, as the
// strs package does for the string iterators. A chain therefore runs each
// step over the whole input before the next step starts. Where that order
// is visible (a closure with side effects followed by a short-circuiting
// step) or the input is large, write the loop instead, as the translation
// rules say; the consuming functions here (Any, All, Find, Position,
// TakeWhile, TryMap, ...) stop where Rust stops.
//
// The functions are named after the Rust methods. A name rs already uses
// takes the prefix Iter: rs.IterMin and rs.IterMax (rs.Min and rs.Max are
// cmp::min and cmp::max), rs.IterTake (rs.Take is mem::take). Elements are
// copied as Go copies them, which is a Rust move; `.cloned()` is Clone.
// Comparisons of keys use Cmp and equality uses Eq. A closure gets the
// element by value.

// Position is `v.iter().position(pred)`: the index of the first element for
// which pred holds.
func Position[T any](v []T, pred func(T) bool) Option[int] {
	for i, x := range v {
		if pred(x) {
			return Some(i)
		}
	}
	return None[int]()
}

// Rposition is `v.iter().rposition(pred)`: the index of the last element
// for which pred holds, searching from the end.
func Rposition[T any](v []T, pred func(T) bool) Option[int] {
	for i := len(v) - 1; i >= 0; i-- {
		if pred(v[i]) {
			return Some(i)
		}
	}
	return None[int]()
}

// Find is `v.iter().find(pred)`: the first element for which pred holds.
func Find[T any](v []T, pred func(T) bool) Option[T] {
	for _, x := range v {
		if pred(x) {
			return Some(x)
		}
	}
	return None[T]()
}

// Rfind is `v.iter().rfind(pred)`: the last element for which pred holds,
// searching from the end.
func Rfind[T any](v []T, pred func(T) bool) Option[T] {
	for i := len(v) - 1; i >= 0; i-- {
		if pred(v[i]) {
			return Some(v[i])
		}
	}
	return None[T]()
}

// FindMap is `v.iter().find_map(f)`: the first Some that f returns.
func FindMap[T, U any](v []T, f func(T) Option[U]) Option[U] {
	for _, x := range v {
		if result := f(x); result.IsSome() {
			return result
		}
	}
	return None[U]()
}

// Any is `v.iter().any(pred)`; it stops at the first element for which
// pred holds.
func Any[T any](v []T, pred func(T) bool) bool {
	for _, x := range v {
		if pred(x) {
			return true
		}
	}
	return false
}

// All is `v.iter().all(pred)`; it stops at the first element for which
// pred does not hold.
func All[T any](v []T, pred func(T) bool) bool {
	for _, x := range v {
		if !pred(x) {
			return false
		}
	}
	return true
}

// Count is `v.iter().filter(pred).count()`: how many elements pred holds
// for. (`count()` alone is len.)
func Count[T any](v []T, pred func(T) bool) int {
	n := 0
	for _, x := range v {
		if pred(x) {
			n++
		}
	}
	return n
}

// Sum is `v.iter().sum()` for a fixed-width integer type. Overflow panics
// with Rust's message when DebugAssertions is on, and wraps when it is off.
func Sum[T FixedInt](v []T) T {
	var sum T
	for _, x := range v {
		sum = addChecked(1, sum, x)
	}
	return sum
}

// Product is `v.iter().product()` for a fixed-width integer type, starting
// at 1. Overflow panics with Rust's message when DebugAssertions is on.
func Product[T FixedInt](v []T) T {
	product := T(1)
	for _, x := range v {
		product = mulChecked(1, product, x)
	}
	return product
}

// UsizeSum is `v.iter().sum::<usize>()`.
func UsizeSum[T ~int](v []T) T {
	var sum uint64
	for _, x := range v {
		next, ok := usizeAdd(sum, usizeOf(1, x))
		sum = uint64(usizeChecked[T](1, next, ok, panicAddOverflow))
	}
	return T(sum)
}

// IsizeSum is `v.iter().sum::<isize>()`.
func IsizeSum[T ~int](v []T) T {
	var sum T
	for _, x := range v {
		sum = addChecked(1, sum, x)
	}
	return sum
}

// UsizeProduct is `v.iter().product::<usize>()`.
func UsizeProduct[T ~int](v []T) T {
	product := uint64(1)
	for _, x := range v {
		next, ok := usizeMul(product, usizeOf(1, x))
		product = uint64(usizeChecked[T](1, next, ok, panicMulOverflow))
	}
	return T(product)
}

// IsizeProduct is `v.iter().product::<isize>()`.
func IsizeProduct[T ~int](v []T) T {
	product := T(1)
	for _, x := range v {
		product = mulChecked(1, product, x)
	}
	return product
}

func mulChecked[T anyInt](skip int, a, b T) T {
	product, overflow := overflowingMul(a, b)
	if overflow && DebugAssertions {
		PanicAt(skip+1, panicMulOverflow)
	}
	return product
}

// F64Sum is `v.iter().sum::<f64>()`. Rust starts the sum at -0.0, so the
// sum of nothing is -0.0 (which prints as "-0").
func F64Sum(v []float64) float64 {
	sum := math.Copysign(0, -1)
	for _, x := range v {
		sum += x
	}
	return sum
}

// F32Sum is `v.iter().sum::<f32>()`, starting at -0.0 as F64Sum does.
func F32Sum(v []float32) float32 {
	sum := float32(math.Copysign(0, -1))
	for _, x := range v {
		sum += x
	}
	return sum
}

// F64Product is `v.iter().product::<f64>()`, starting at 1.0.
func F64Product(v []float64) float64 {
	product := 1.0
	for _, x := range v {
		product *= x
	}
	return product
}

// F32Product is `v.iter().product::<f32>()`, starting at 1.0.
func F32Product(v []float32) float32 {
	product := float32(1)
	for _, x := range v {
		product *= x
	}
	return product
}

// IterMin is `v.iter().min()`: the first of the least elements.
func IterMin[T cmp.Ordered](v []T) Option[T] {
	return MinBy(v, cmp.Compare[T])
}

// IterMax is `v.iter().max()`: the last of the greatest elements.
func IterMax[T cmp.Ordered](v []T) Option[T] {
	return MaxBy(v, cmp.Compare[T])
}

// MinBy is `v.iter().min_by(compare)`: the first element that no later one
// is less than. compare(kept, next) is called for each next element.
func MinBy[T any, O CompareResult](v []T, compare func(a, b T) O) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	kept := v[0]
	for _, x := range v[1:] {
		// cmp::min_by keeps the first unless it is greater.
		if !(compare(kept, x) <= 0) {
			kept = x
		}
	}
	return Some(kept)
}

// MaxBy is `v.iter().max_by(compare)`: the last element that no later one
// is greater than. compare(kept, next) is called for each next element.
func MaxBy[T any, O CompareResult](v []T, compare func(a, b T) O) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	kept := v[0]
	for _, x := range v[1:] {
		// cmp::max_by keeps the first only when it is greater.
		if !(compare(kept, x) > 0) {
			kept = x
		}
	}
	return Some(kept)
}

// MinByKey is `v.iter().min_by_key(key)`: the first element with the least
// key. key is called once for each element, in order, and the keys are
// compared with Cmp.
func MinByKey[T, K any](v []T, key func(T) K) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	kept, keptKey := v[0], key(v[0])
	for _, x := range v[1:] {
		xKey := key(x)
		if !(Cmp(keptKey, xKey) <= 0) {
			kept, keptKey = x, xKey
		}
	}
	return Some(kept)
}

// MaxByKey is `v.iter().max_by_key(key)`: the last element with the
// greatest key. key is called once for each element, in order.
func MaxByKey[T, K any](v []T, key func(T) K) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	kept, keptKey := v[0], key(v[0])
	for _, x := range v[1:] {
		xKey := key(x)
		if !(Cmp(keptKey, xKey) > 0) {
			kept, keptKey = x, xKey
		}
	}
	return Some(kept)
}

// Fold is `v.iter().fold(init, f)`.
func Fold[T, A any](v []T, init A, f func(A, T) A) A {
	acc := init
	for _, x := range v {
		acc = f(acc, x)
	}
	return acc
}

// Reduce is `v.iter().reduce(f)`: None for no elements, else the first
// element folded with the rest.
func Reduce[T any](v []T, f func(T, T) T) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	return Some(Fold(v[1:], v[0], f))
}

// TryFold is `v.iter().try_fold(init, f)` with f returning a Result: it
// stops at the first error.
func TryFold[T, A any](v []T, init A, f func(A, T) (A, error)) (A, error) {
	acc := init
	for _, x := range v {
		next, err := f(acc, x)
		if err != nil {
			return next, err
		}
		acc = next
	}
	return acc, nil
}

// TryForEach is `v.iter().try_for_each(f)` with f returning a Result: it
// stops at the first error.
func TryForEach[T any](v []T, f func(T) error) error {
	for _, x := range v {
		if err := f(x); err != nil {
			return err
		}
	}
	return nil
}

// Map is `v.iter().map(f).collect::<Vec<_>>()`.
func Map[T, U any](v []T, f func(T) U) []U {
	out := make([]U, len(v))
	for i, x := range v {
		out[i] = f(x)
	}
	return out
}

// TryMap is `v.iter().map(f).collect::<Result<Vec<_>, _>>()`: it stops at
// the first error and returns it.
func TryMap[T, U any](v []T, f func(T) (U, error)) ([]U, error) {
	out := make([]U, 0, len(v))
	for _, x := range v {
		y, err := f(x)
		if err != nil {
			return nil, err
		}
		out = append(out, y)
	}
	return out, nil
}

// MapOption is `v.iter().map(f).collect::<Option<Vec<_>>>()`: None as soon
// as f returns None.
func MapOption[T, U any](v []T, f func(T) Option[U]) Option[[]U] {
	out := make([]U, 0, len(v))
	for _, x := range v {
		y, ok := f(x).Get()
		if !ok {
			return None[[]U]()
		}
		out = append(out, y)
	}
	return Some(out)
}

// Filter is `v.iter().filter(pred).collect::<Vec<_>>()`.
func Filter[T any](v []T, pred func(T) bool) []T {
	out := []T{}
	for _, x := range v {
		if pred(x) {
			out = append(out, x)
		}
	}
	return out
}

// FilterMap is `v.iter().filter_map(f).collect::<Vec<_>>()`.
func FilterMap[T, U any](v []T, f func(T) Option[U]) []U {
	out := []U{}
	for _, x := range v {
		if y, ok := f(x).Get(); ok {
			out = append(out, y)
		}
	}
	return out
}

// FlatMap is `v.iter().flat_map(f).collect::<Vec<_>>()`.
func FlatMap[T, U any](v []T, f func(T) []U) []U {
	out := []U{}
	for _, x := range v {
		out = append(out, f(x)...)
	}
	return out
}

// Flatten is `v.into_iter().flatten().collect::<Vec<_>>()` over slices.
func Flatten[T any](v [][]T) []T {
	out := []T{}
	for _, inner := range v {
		out = append(out, inner...)
	}
	return out
}

// FlattenOptions is `v.into_iter().flatten().collect::<Vec<_>>()` over
// options: the values of the ones that are Some.
func FlattenOptions[T any](v []Option[T]) []T {
	out := []T{}
	for _, o := range v {
		if x, ok := o.Get(); ok {
			out = append(out, x)
		}
	}
	return out
}

// Zip is `a.iter().zip(b).collect::<Vec<_>>()`: pairs, as many as the
// shorter input has.
func Zip[A, B any](a []A, b []B) []Tuple2[A, B] {
	n := min(len(a), len(b))
	out := make([]Tuple2[A, B], n)
	for i := range n {
		out[i] = T2(a[i], b[i])
	}
	return out
}

// Unzip is `v.into_iter().unzip()`.
func Unzip[A, B any](v []Tuple2[A, B]) ([]A, []B) {
	a := make([]A, len(v))
	b := make([]B, len(v))
	for i, pair := range v {
		a[i], b[i] = pair.F0, pair.F1
	}
	return a, b
}

// Enumerate is `v.iter().enumerate().collect::<Vec<_>>()`.
func Enumerate[T any](v []T) []Tuple2[int, T] {
	out := make([]Tuple2[int, T], len(v))
	for i, x := range v {
		out[i] = T2(i, x)
	}
	return out
}

// Skip is `v.iter().skip(n)`: the elements after the first n.
func Skip[T any](v []T, n int) []T {
	if n >= len(v) {
		return v[len(v):]
	}
	return v[n:]
}

// IterTake is `v.iter().take(n)`: the first n elements, or all of them.
func IterTake[T any](v []T, n int) []T {
	if n >= len(v) {
		return v
	}
	return v[:n]
}

// StepBy is `v.iter().step_by(step)`: the first element and every step-th
// after it. It panics for a step of zero, as Rust does.
func StepBy[T any](v []T, step int) []T {
	if !(step != 0) {
		PanicAt(1, "assertion failed: step != 0")
	}
	out := make([]T, 0, (len(v)+step-1)/step)
	for i := 0; i < len(v); i += step {
		out = append(out, v[i])
	}
	return out
}

// SkipWhile is `v.iter().skip_while(pred)`: the elements from the first
// one pred does not hold for. pred is not called after that.
func SkipWhile[T any](v []T, pred func(T) bool) []T {
	for i, x := range v {
		if !pred(x) {
			return v[i:]
		}
	}
	return v[len(v):]
}

// TakeWhile is `v.iter().take_while(pred)`: the elements before the first
// one pred does not hold for. pred is not called after that.
func TakeWhile[T any](v []T, pred func(T) bool) []T {
	for i, x := range v {
		if !pred(x) {
			return v[:i]
		}
	}
	return v
}

// MapWhile is `v.iter().map_while(f).collect::<Vec<_>>()`: the values f
// returns until its first None.
func MapWhile[T, U any](v []T, f func(T) Option[U]) []U {
	out := []U{}
	for _, x := range v {
		y, ok := f(x).Get()
		if !ok {
			break
		}
		out = append(out, y)
	}
	return out
}

// Scan is `v.iter().scan(initial, f).collect::<Vec<_>>()`: f gets the state
// and each element, and the values it returns are collected until its
// first None.
func Scan[T, S, U any](v []T, initial S, f func(*S, T) Option[U]) []U {
	state := initial
	out := []U{}
	for _, x := range v {
		y, ok := f(&state, x).Get()
		if !ok {
			break
		}
		out = append(out, y)
	}
	return out
}

// Partition is `v.iter().partition(pred)`: the elements pred holds for,
// and the others, each in order.
func Partition[T any](v []T, pred func(T) bool) ([]T, []T) {
	yes, no := []T{}, []T{}
	for _, x := range v {
		if pred(x) {
			yes = append(yes, x)
		} else {
			no = append(no, x)
		}
	}
	return yes, no
}

// Nth is `v.iter().nth(n)`: the element at index n, or None.
func Nth[T any](v []T, n int) Option[T] {
	return Get(v, n)
}

// Rev is `v.iter().rev().collect::<Vec<_>>()`: a reversed copy of v.
func Rev[T any](v []T) []T {
	out := make([]T, len(v))
	for i, x := range v {
		out[len(v)-1-i] = x
	}
	return out
}

// Chain is `a.iter().chain(b)...` over any number of slices: their elements
// in order, in a new slice.
func Chain[T any](parts ...[]T) []T {
	out := []T{}
	for _, part := range parts {
		out = append(out, part...)
	}
	return out
}

// RepeatN is `iter::repeat_n(element, n).collect::<Vec<_>>()`: n - 1
// Clones of element and element itself last.
func RepeatN[T any](element T, n int) []T {
	out := make([]T, 0, max(n, 0))
	for i := 0; i < n; i++ {
		if i == n-1 {
			out = append(out, element)
		} else {
			out = append(out, Clone(element))
		}
	}
	return out
}
