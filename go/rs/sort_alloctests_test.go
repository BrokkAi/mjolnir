// Port of alloctests 1.99.1 tests/sort/tests.rs and tests/sort/patterns.rs:
// the properties every sort must have, run for the stable sort
// (SortStableBy) and the unstable one (SortUnstableBy).
//
// Left out: lengths of 1_100_000 (100_000 is the longest here); the
// `observable_is_less`, `panic_observable_is_less` and `self_cmp` tests,
// which watch the comparison change elements through a Cell or compare two
// references by address: the Go functions pass the comparison copies of the
// elements, so neither is visible; `fixed_seed`, which tests the Rust test
// harness's seed. The patterns use Go's PCG generator and a Zipf sampler of
// their own in place of rand_xorshift and the crate's zipf.rs, since the
// tests check properties of the result, not particular inputs.

package rs_test

import (
	"cmp"
	"fmt"
	"math"
	"math/rand/v2"
	"slices"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type sortTestSort struct {
	name     string
	unstable bool
}

var sortTestSorts = []sortTestSort{{"stable", false}, {"unstable", true}}

func sortTestSortBy[T any](s sortTestSort, v []T, compare func(a, b T) int) {
	if s.unstable {
		rs.SortUnstableBy(v, compare)
	} else {
		rs.SortStableBy(v, compare)
	}
}

// --- patterns ---

const sortTestSeed = 0x5eed

func sortTestRng() *rand.Rand { return rand.New(rand.NewPCG(sortTestSeed, sortTestSeed)) }

func sortTestRandom(n int) []int32 {
	rng := sortTestRng()
	v := make([]int32, n)
	for i := range v {
		v[i] = int32(rng.Uint32())
	}
	return v
}

// sortTestRandomUniform draws from [lo, hi).
func sortTestRandomUniform(n int, lo, hi int32) []int32 {
	rng := sortTestRng()
	v := make([]int32, n)
	for i := range v {
		v[i] = lo + rng.Int32N(hi-lo)
	}
	return v
}

func sortTestRandomZipf(n int, exponent float64) []int32 {
	if n == 0 {
		return []int32{}
	}
	cumulative := make([]float64, n)
	total := 0.0
	for k := 1; k <= n; k++ {
		total += 1 / math.Pow(float64(k), exponent)
		cumulative[k-1] = total
	}
	rng := sortTestRng()
	v := make([]int32, n)
	for i := range v {
		target := rng.Float64() * total
		k, _ := slices.BinarySearch(cumulative, target)
		v[i] = int32(min(k, n-1) + 1)
	}
	return v
}

func sortTestRandomSorted(n int, sortedPercent float64) []int32 {
	v := sortTestRandom(n)
	sortedLen := int(math.Round(float64(n) * (sortedPercent / 100)))
	slices.Sort(v[:sortedLen])
	return v
}

func sortTestAllEqual(n int) []int32 {
	v := make([]int32, n)
	for i := range v {
		v[i] = 66
	}
	return v
}

func sortTestAscending(n int) []int32 {
	v := make([]int32, n)
	for i := range v {
		v[i] = int32(i)
	}
	return v
}

func sortTestDescending(n int) []int32 {
	v := sortTestAscending(n)
	slices.Reverse(v)
	return v
}

func sortTestSawMixed(n, sawCount int) []int32 {
	if n == 0 {
		return []int32{}
	}
	v := sortTestRandom(n)
	chunkSize := n / max(sawCount, 1)
	directions := sortTestRandomUniform(n/chunkSize+1, 0, 2)
	for i := 0; i*chunkSize < n; i++ {
		chunk := v[i*chunkSize : min((i+1)*chunkSize, n)]
		if directions[i] == 0 {
			slices.Sort(chunk)
		} else {
			slices.SortFunc(chunk, func(a, b int32) int { return cmp.Compare(b, a) })
		}
	}
	return v
}

func sortTestSawMixedRange(n, lo, hi int) []int32 {
	if n == 0 {
		return []int32{}
	}
	v := sortTestRandom(n)
	maxChunks := n / lo
	directions := sortTestRandomUniform(maxChunks+1, 0, 2)
	sizes := sortTestRandomUniform(maxChunks+1, int32(lo), int32(hi))
	for i, l := 0, 0; l < n; i++ {
		chunk := v[l:min(l+int(sizes[i]), n)]
		if directions[i] == 0 {
			slices.Sort(chunk)
		} else {
			slices.SortFunc(chunk, func(a, b int32) int { return cmp.Compare(b, a) })
		}
		l += int(sizes[i])
	}
	return v
}

func sortTestPipeOrgan(n int) []int32 {
	v := sortTestRandom(n)
	slices.Sort(v[:n/2])
	slices.SortFunc(v[n/2:], func(a, b int32) int { return cmp.Compare(b, a) })
	return v
}

type sortTestPattern struct {
	name string
	gen  func(n int) []int32
}

func sortTestLog2(n int) float64 { return math.Round(math.Log2(float64(n))) }

var sortTestDefaultPatterns = []sortTestPattern{
	{"random", sortTestRandom},
	{"random_z1", func(n int) []int32 { return sortTestRandomZipf(n, 1.0) }},
	{"random_d2", func(n int) []int32 { return sortTestRandomUniform(n, 0, 2) }},
	{"random_d20", func(n int) []int32 { return sortTestRandomUniform(n, 0, 16) }},
	{"random_s95", func(n int) []int32 { return sortTestRandomSorted(n, 95) }},
	{"ascending", sortTestAscending},
	{"descending", sortTestDescending},
	{"saw_mixed", func(n int) []int32 { return sortTestSawMixed(n, int(sortTestLog2(n))) }},
}

var sortTestI32Patterns = append(slices.Clone(sortTestDefaultPatterns), []sortTestPattern{
	{"random_d4", func(n int) []int32 { return sortTestRandomUniform(n, 0, 4) }},
	{"random_d8", func(n int) []int32 { return sortTestRandomUniform(n, 0, 8) }},
	{"random_d311", func(n int) []int32 { return sortTestRandomUniform(n, 0, 311) }},
	{"random_d1024", func(n int) []int32 { return sortTestRandomUniform(n, 0, 1024) }},
	{"random_z1_03", func(n int) []int32 { return sortTestRandomZipf(n, 1.03) }},
	{"random_z2", func(n int) []int32 { return sortTestRandomZipf(n, 2.0) }},
	{"random_s50", func(n int) []int32 { return sortTestRandomSorted(n, 50) }},
	{"narrow", func(n int) []int32 { return sortTestRandomUniform(n, 0, int32(sortTestLog2(n))*100+1) }},
	{"all_equal", sortTestAllEqual},
	{"saw_mixed_range", func(n int) []int32 { return sortTestSawMixedRange(n, 20, 50) }},
	{"pipe_organ", sortTestPipeOrgan},
}...)

// sortTestLengths is TEST_LENGTHS without 1_100_000.
var sortTestLengths = []int{
	2, 3, 4, 5, 6, 7, 8, 9, 10, 15, 16, 17, 20, 24, 30, 32, 33, 35, 50, 100, 200, 500, 1_000,
	2_048, 5_000, 10_000, 100_000,
}

// The lengths Rust writes as &TEST_LENGTHS[..TEST_LENGTHS.len() - 2] and
// &TEST_LENGTHS[..TEST_LENGTHS.len() - 3].
var (
	sortTestLengthsUpTo10k = sortTestLengths[:len(sortTestLengths)-1]
	sortTestLengthsUpTo5k  = sortTestLengths[:len(sortTestLengths)-2]
)

// --- element types ---

type sortTestType[T any] struct {
	name    string
	into    func(int32) T
	from    func(T) int32
	compare func(a, b T) int
}

func sortTestShiftI32ToU32(val int32) uint32 { return uint32(int64(val) + (math.MaxInt32 + 1)) }

var (
	sortTestI32     = sortTestType[int32]{"i32", func(v int32) int32 { return v }, func(v int32) int32 { return v }, cmp.Compare[int32]}
	sortTestCellI32 = sortTestType[rs.Cell[int32]]{"cell_i32", rs.CellNew[int32],
		func(c rs.Cell[int32]) int32 { return c.Get() },
		func(a, b rs.Cell[int32]) int { return cmp.Compare(a.Get(), b.Get()) }}
	sortTestString = sortTestType[string]{"string",
		func(v int32) string { return fmt.Sprintf("%010d", sortTestShiftI32ToU32(v)) },
		func(s string) int32 {
			u, err := strconv.ParseUint(s, 10, 32)
			if err != nil {
				panic(err)
			}
			return int32(int64(u) - (math.MaxInt32 + 1))
		},
		cmp.Compare[string]}
)

// --- the checks ---

// sortTestCheckIsSorted is check_is_sorted: v sorted is in order.
func sortTestCheckIsSorted[T any](t *testing.T, s sortTestSort, v []T, compare func(a, b T) int) {
	t.Helper()
	original := slices.Clone(v)
	sortTestSortBy(s, v, compare)
	if len(v) != len(original) {
		t.Fatalf("%s: length changed from %d to %d", s.name, len(original), len(v))
	}
	for i := 1; i < len(v); i++ {
		if compare(v[i-1], v[i]) > 0 {
			expected := slices.Clone(original)
			slices.SortStableFunc(expected, compare)
			if len(v) <= 100 {
				t.Fatalf("%s: not sorted\noriginal: %v\nexpected: %v\ngot:      %v", s.name, original, expected, v)
			}
			t.Fatalf("%s: not sorted at %d (length %d)", s.name, i, len(v))
		}
	}
}

func sortTestCorrect[T any](t *testing.T, ty sortTestType[T], patterns []sortTestPattern, lengths []int) {
	for _, s := range sortTestSorts {
		for _, pattern := range patterns {
			for _, n := range lengths {
				input := pattern.gen(n)
				v := make([]T, n)
				for i, x := range input {
					v[i] = ty.into(x)
				}
				t.Run(fmt.Sprintf("%s/%s/%s/%d", s.name, ty.name, pattern.name, n), func(t *testing.T) {
					sortTestCheckIsSorted(t, s, v, ty.compare)
				})
			}
		}
	}
}

func TestSortTests_basic(t *testing.T) {
	for _, s := range sortTestSorts {
		sortTestCheckIsSorted(t, s, []int32{}, cmp.Compare[int32])
		unit := func(a, b struct{}) int { return 0 }
		sortTestCheckIsSorted(t, s, []struct{}{}, unit)
		sortTestCheckIsSorted(t, s, []struct{}{{}}, unit)
		sortTestCheckIsSorted(t, s, []struct{}{{}, {}}, unit)
		sortTestCheckIsSorted(t, s, []struct{}{{}, {}, {}}, unit)
		sortTestCheckIsSorted(t, s, []int32{77}, cmp.Compare[int32])
		sortTestCheckIsSorted(t, s, []int32{2, 3}, cmp.Compare[int32])
		sortTestCheckIsSorted(t, s, []int32{2, 3, 6}, cmp.Compare[int32])
		sortTestCheckIsSorted(t, s, []int32{2, 3, 99, 6}, cmp.Compare[int32])
		sortTestCheckIsSorted(t, s, []int32{2, 7709, 400, 90932}, cmp.Compare[int32])
		sortTestCheckIsSorted(t, s, []int32{15, -1, 3, -1, -3, -1, 7}, cmp.Compare[int32])
	}
}

func TestSortTests_fixed_seed_rand_vec_prefix(t *testing.T) {
	if !slices.Equal(sortTestRandom(5), sortTestRandom(7)[:5]) {
		t.Fatal("random(5) is not a prefix of random(7)")
	}
}

func TestSortTests_int_edge(t *testing.T) {
	for _, s := range sortTestSorts {
		i32s := [][]int32{
			{math.MinInt32, math.MaxInt32},
			{math.MaxInt32, math.MinInt32},
			{math.MinInt32, 3},
			{math.MinInt32, -3},
			{math.MinInt32, -3, math.MaxInt32},
			{math.MinInt32, -3, math.MaxInt32, math.MinInt32, 5},
			{math.MaxInt32, 3, math.MinInt32, 5, math.MinInt32, -3, 60, 200, 50, 7, 10},
		}
		for _, v := range i32s {
			sortTestCheckIsSorted(t, s, v, cmp.Compare[int32])
		}
		u64s := [][]uint64{
			{0, math.MaxUint64},
			{math.MaxUint64, 0},
			{0, 3},
			{0, math.MaxUint64 - 3},
			{0, math.MaxUint64 - 3, math.MaxUint64},
			{0, math.MaxUint64 - 3, math.MaxUint64, 0, 5},
			{math.MaxUint64, 3, 0, 5, 0, math.MaxUint64 - 3, 60, 200, 50, 7, 10},
		}
		for _, v := range u64s {
			sortTestCheckIsSorted(t, s, v, cmp.Compare[uint64])
		}
		large := sortTestRandom(sortTestLengths[len(sortTestLengths)-1])
		large = append(large, math.MaxInt32, math.MinInt32, math.MaxInt32)
		sortTestCheckIsSorted(t, s, large, cmp.Compare[int32])
	}
}

func TestSortTests_sort_vs_sort_by(t *testing.T) {
	expected := []int32{-801, -801, -3, 3, 5, 7, 10, 50, 60, 200, 800}
	for _, s := range sortTestSorts {
		inputNormal := []int32{800, 3, -801, 5, -801, -3, 60, 200, 50, 7, 10}
		inputSortBy := slices.Clone(inputNormal)
		if s.unstable {
			rs.SortUnstable(inputNormal)
			rs.SortUnstableBy(inputSortBy, func(a, b int32) rs.Ordering { return rs.OrdCmp(a, b) })
		} else {
			rs.SortStable(inputNormal)
			rs.SortStableBy(inputSortBy, func(a, b int32) rs.Ordering { return rs.OrdCmp(a, b) })
		}
		if !slices.Equal(inputNormal, expected) || !slices.Equal(inputSortBy, expected) {
			t.Fatalf("%s: got %v and %v, want %v", s.name, inputNormal, inputSortBy, expected)
		}
	}
}

func TestSortTests_box_value(t *testing.T) {
	for _, s := range sortTestSorts {
		for _, n := range []int{3, 9, 35, 56, 132} {
			for _, input := range [][]int32{sortTestRandom(n), sortTestRandomSorted(n, 80)} {
				v := make([]*int32, len(input))
				for i := range input {
					v[i] = &input[i]
				}
				sortTestCheckIsSorted(t, s, v, func(a, b *int32) int { return cmp.Compare(*a, *b) })
			}
		}
	}
}

func TestSortTests_correct_i32(t *testing.T) {
	sortTestCorrect(t, sortTestI32, sortTestI32Patterns, sortTestLengths)
}

func TestSortTests_correct_u64(t *testing.T) {
	ty := sortTestType[uint64]{"u64", func(v int32) uint64 { return uint64(sortTestShiftI32ToU32(v)) * math.MaxInt32 },
		nil, cmp.Compare[uint64]}
	sortTestCorrect(t, ty, sortTestDefaultPatterns, sortTestLengths)
}

func TestSortTests_correct_u128(t *testing.T) {
	ty := sortTestType[rs.U128]{"u128", func(v int32) rs.U128 {
		return rs.U128FromU64(uint64(sortTestShiftI32ToU32(v))).Mul(rs.U128FromU64(math.MaxInt64))
	}, nil, func(a, b rs.U128) int { return a.Compare(b) }}
	sortTestCorrect(t, ty, sortTestDefaultPatterns, sortTestLengthsUpTo10k)
}

func TestSortTests_correct_cell_i32(t *testing.T) {
	sortTestCorrect(t, sortTestCellI32, sortTestDefaultPatterns, sortTestLengthsUpTo10k)
}

func TestSortTests_correct_string(t *testing.T) {
	sortTestCorrect(t, sortTestString, sortTestDefaultPatterns, sortTestLengthsUpTo10k)
}

// sortTestF128 is ffi_types.rs's F128, ordered by x / y.
type sortTestF128 struct{ x, y float64 }

func TestSortTests_correct_f128(t *testing.T) {
	ty := sortTestType[sortTestF128]{"f128", func(val int32) sortTestF128 {
		valF := float64(val) + math.MaxInt32 + 10
		return sortTestF128{x: valF + 0.1, y: math.Log(valF) / math.Log(4.1)}
	}, nil, func(a, b sortTestF128) int { return cmp.Compare(a.x/a.y, b.x/b.y) }}
	sortTestCorrect(t, ty, sortTestDefaultPatterns, sortTestLengthsUpTo10k)
}

// sortTestOneKibiByte is ffi_types.rs's FFIOneKibiByte.
type sortTestOneKibiByte struct{ values [128]int64 }

func (v *sortTestOneKibiByte) asI64() int64 { return v.values[11] + v.values[55] + v.values[77] }

func TestSortTests_correct_1k(t *testing.T) {
	ty := sortTestType[sortTestOneKibiByte]{"1k", func(val int32) sortTestOneKibiByte {
		var v sortTestOneKibiByte
		next := int64(val)
		for i := range v.values {
			v.values[i] = next
			next++
		}
		return v
	}, nil, func(a, b sortTestOneKibiByte) int { return cmp.Compare(a.asI64(), b.asI64()) }}
	sortTestCorrect(t, ty, sortTestDefaultPatterns, sortTestLengthsUpTo10k)
}

type sortTestDyn interface{ getVal() int32 }

type sortTestDynValA struct{ value int32 }

type sortTestDynValB struct{ value uint64 }

func (v *sortTestDynValA) getVal() int32 { return v.value }

func (v *sortTestDynValB) getVal() int32 {
	// i32::from_ne_bytes([bytes[0], bytes[1], bytes[6], bytes[7]]) on a
	// little-endian machine.
	return int32(uint32(v.value&0xffff) | uint32(v.value>>48)<<16)
}

func TestSortTests_correct_dyn_val(t *testing.T) {
	ty := sortTestType[sortTestDyn]{"dyn_val", func(val int32) sortTestDyn {
		if val%2 == 0 {
			return &sortTestDynValA{val}
		}
		return &sortTestDynValB{uint64(sortTestShiftI32ToU32(val)) * math.MaxInt32}
	}, nil, func(a, b sortTestDyn) int { return cmp.Compare(a.getVal(), b.getVal()) }}
	sortTestCorrect(t, ty, sortTestDefaultPatterns, sortTestLengthsUpTo10k)
}

// sortTestThreeTypes runs a property for i32, Cell<i32> and String, as
// gen_sort_test_fns_with_default_patterns_3_ty does.
func sortTestThreeTypes(t *testing.T, check func(t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty string)) {
	for _, s := range sortTestSorts {
		for _, pattern := range sortTestDefaultPatterns {
			for _, ty := range []string{"i32", "cell_i32", "string"} {
				lengths := sortTestLengthsUpTo10k
				if ty != "i32" {
					lengths = sortTestLengthsUpTo5k
				}
				for _, n := range lengths {
					check(t, s, n, pattern, ty)
				}
			}
		}
	}
}

func sortTestRun3[R any](ty string, run func(sortTestType[int32]) R, cell func(sortTestType[rs.Cell[int32]]) R, str func(sortTestType[string]) R) R {
	switch ty {
	case "i32":
		return run(sortTestI32)
	case "cell_i32":
		return cell(sortTestCellI32)
	}
	return str(sortTestString)
}

func sortTestStability[T any](t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty sortTestType[T]) bool {
	if s.unstable {
		return true
	}
	var counts [128]int32
	orig := make([]rs.Tuple2[T, int32], 0, n)
	for _, val := range pattern.gen(n) {
		m := rs.SaturatingAbs(val) % int32(len(counts))
		counts[m]++
		orig = append(orig, rs.T2(ty.into(m), counts[m]))
	}
	v := slices.Clone(orig)
	compare := func(a, b rs.Tuple2[T, int32]) int {
		if order := ty.compare(a.F0, b.F0); order != 0 {
			return order
		}
		return cmp.Compare(a.F1, b.F1)
	}
	sortTestSortBy(s, v, compare)
	for i := 1; i < len(v); i++ {
		if compare(v[i-1], v[i]) > 0 {
			t.Errorf("%s/%s/%s/%d: not in order", s.name, ty.name, pattern.name, n)
			return false
		}
	}
	return true
}

func TestSortTests_stability(t *testing.T) {
	sortTestThreeTypes(t, func(t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty string) {
		sortTestRun3(ty,
			func(ty sortTestType[int32]) bool { return sortTestStability(t, s, n, pattern, ty) },
			func(ty sortTestType[rs.Cell[int32]]) bool { return sortTestStability(t, s, n, pattern, ty) },
			func(ty sortTestType[string]) bool { return sortTestStability(t, s, n, pattern, ty) })
	})
}

func sortTestSum[T any](v []T, from func(T) int32) int64 {
	sum := int64(0)
	for _, x := range v {
		sum += int64(from(x))
	}
	return sum
}

func sortTestCompsRequired[T any](s sortTestSort, v []T, compare func(a, b T) int) int {
	count := 0
	sortTestSortBy(s, v, func(a, b T) int {
		count++
		return compare(a, b)
	})
	return count
}

func sortTestPanicRetainOrigSet[T any](t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty sortTestType[T], rng *rand.Rand) bool {
	v := make([]T, n)
	for i, x := range pattern.gen(n) {
		v[i] = ty.into(x)
	}
	sumBefore := sortTestSum(v, ty.from)
	required := sortTestCompsRequired(s, slices.Clone(v), ty.compare)
	threshold := rng.IntN(required)
	counter := 0
	panicked := func() (panicked bool) {
		defer func() { panicked = recover() != nil }()
		sortTestSortBy(s, v, func(a, b T) int {
			if counter == threshold {
				panic("comparison panics")
			}
			counter++
			return ty.compare(a, b)
		})
		return false
	}()
	if !panicked {
		t.Errorf("%s/%s/%s/%d: the comparison did not panic", s.name, ty.name, pattern.name, n)
		return false
	}
	if sumAfter := sortTestSum(v, ty.from); sumAfter != sumBefore {
		t.Errorf("%s/%s/%s/%d: elements changed after a panic: sum %d, was %d", s.name, ty.name, pattern.name, n, sumAfter, sumBefore)
		return false
	}
	return true
}

func TestSortTests_panic_retain_orig_set(t *testing.T) {
	rng := sortTestRng()
	sortTestThreeTypes(t, func(t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty string) {
		sortTestRun3(ty,
			func(ty sortTestType[int32]) bool { return sortTestPanicRetainOrigSet(t, s, n, pattern, ty, rng) },
			func(ty sortTestType[rs.Cell[int32]]) bool {
				return sortTestPanicRetainOrigSet(t, s, n, pattern, ty, rng)
			},
			func(ty sortTestType[string]) bool { return sortTestPanicRetainOrigSet(t, s, n, pattern, ty, rng) })
	})
}

func sortTestDeterministic[T any](t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty sortTestType[T]) bool {
	v := make([]T, n)
	for i, x := range pattern.gen(n) {
		v[i] = ty.into(x)
	}
	clone := slices.Clone(v)
	compare := func(a, b T) int { return cmp.Compare(ty.from(a)%10_000, ty.from(b)%10_000) }
	sortTestSortBy(s, v, compare)
	sortTestSortBy(s, clone, compare)
	for i := range v {
		if ty.from(v[i]) != ty.from(clone[i]) {
			t.Errorf("%s/%s/%s/%d: two sorts of one input differ at %d", s.name, ty.name, pattern.name, n, i)
			return false
		}
	}
	return true
}

func TestSortTests_deterministic(t *testing.T) {
	sortTestThreeTypes(t, func(t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty string) {
		sortTestRun3(ty,
			func(ty sortTestType[int32]) bool { return sortTestDeterministic(t, s, n, pattern, ty) },
			func(ty sortTestType[rs.Cell[int32]]) bool { return sortTestDeterministic(t, s, n, pattern, ty) },
			func(ty sortTestType[string]) bool { return sortTestDeterministic(t, s, n, pattern, ty) })
	})
}

func sortTestViolateOrdRetainOrigSet[T any](t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty sortTestType[T]) bool {
	randomOrderings := sortTestRandomUniform(10_000, 0, 2)
	next := func(index *int) int {
		r := *index
		*index++
		if r+1 == len(randomOrderings) {
			*index = 0
		}
		return int(randomOrderings[r])
	}
	var randomIdxA, randomIdxB, randomIdxC int
	lastElementA, lastElementB := int32(-1), int32(-1)
	var randCounterB, randCounterC, streakCounterA, streakCounterB int
	const streakLen = 50
	comparisons := []func(a, b T) int{
		func(_, _ T) int { return []int{-1, 0, 1}[next(&randomIdxA)] },
		func(_, _ T) int { return -1 },
		func(_, _ T) int { return 0 },
		func(_, _ T) int { return 1 },
		func(a, b T) int {
			if ty.from(a) == ty.from(b) {
				return -1
			}
			return 1
		},
		func(a, b T) int {
			lea, leb := lastElementA, lastElementB
			aI32, bI32 := ty.from(a), ty.from(b)
			lastElementA, lastElementB = aI32, bI32
			if aI32 == lea && bI32 != leb {
				return ty.compare(b, a)
			}
			return ty.compare(a, b)
		},
		func(a, b T) int {
			randCounterB += next(&randomIdxB)
			if randCounterB >= 100 {
				randCounterB = 0
				return ty.compare(b, a)
			}
			return ty.compare(a, b)
		},
		func(a, b T) int {
			randCounterC += next(&randomIdxC)
			if randCounterC >= 3 {
				randCounterC = 0
				return ty.compare(b, a)
			}
			return ty.compare(a, b)
		},
		func(a, b T) int {
			streakCounterA++
			if streakCounterA <= streakLen {
				return ty.compare(a, b)
			}
			if streakCounterA == streakLen*2 {
				streakCounterA = 0
			}
			return -1
		},
		func(a, b T) int {
			streakCounterB++
			if streakCounterB <= streakLen {
				return ty.compare(a, b)
			}
			if streakCounterB == streakLen*2 {
				streakCounterB = 0
			}
			return 1
		},
	}
	for index, compare := range comparisons {
		v := make([]T, n)
		for i, x := range pattern.gen(n) {
			v[i] = ty.into(x)
		}
		sumBefore := sortTestSum(v, ty.from)
		func() {
			defer func() {
				if recovered := recover(); recovered != nil {
					message := rs.PanicInfoOf(recovered).Message
					if message != "user-provided comparison function does not correctly implement a total order" {
						t.Errorf("%s/%s/%s/%d: comparison %d: unexpected panic %q", s.name, ty.name, pattern.name, n, index, message)
					}
				}
			}()
			sortTestSortBy(s, v, compare)
		}()
		if sumAfter := sortTestSum(v, ty.from); sumAfter != sumBefore {
			t.Errorf("%s/%s/%s/%d: comparison %d changed the elements: sum %d, was %d", s.name, ty.name, pattern.name, n, index, sumAfter, sumBefore)
			return false
		}
	}
	return true
}

func TestSortTests_violate_ord_retain_orig_set(t *testing.T) {
	sortTestThreeTypes(t, func(t *testing.T, s sortTestSort, n int, pattern sortTestPattern, ty string) {
		sortTestRun3(ty,
			func(ty sortTestType[int32]) bool { return sortTestViolateOrdRetainOrigSet(t, s, n, pattern, ty) },
			func(ty sortTestType[rs.Cell[int32]]) bool {
				return sortTestViolateOrdRetainOrigSet(t, s, n, pattern, ty)
			},
			func(ty sortTestType[string]) bool { return sortTestViolateOrdRetainOrigSet(t, s, n, pattern, ty) })
	})
}
