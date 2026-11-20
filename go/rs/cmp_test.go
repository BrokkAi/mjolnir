package rs

import (
	"math"
	"slices"
	"strings"
	"testing"
)

// orderingName names what Cmp returned, for the tables below.
func orderingName(order int) string {
	switch order {
	case -1:
		return "Less"
	case 0:
		return "Equal"
	case 1:
		return "Greater"
	}
	return "an int other than -1, 0 and 1"
}

// expectCmp checks Cmp(a, b), and the mirrored Cmp(b, a).
func expectCmp[T any](t *testing.T, name string, a, b T, want Ordering) {
	t.Helper()
	if got := Cmp(a, b); got != int(want) {
		t.Errorf("%s: Cmp = %s, want %s", name, orderingName(got), orderingName(int(want)))
	}
	if got := Cmp(b, a); got != int(want.Reverse()) {
		t.Errorf("%s, swapped: Cmp = %s, want %s", name, orderingName(got), orderingName(int(want.Reverse())))
	}
	if got := OrdCmp(a, b); got != want {
		t.Errorf("%s: OrdCmp = %d, want %d", name, got, want)
	}
}

func TestCmp_test_int_totalord(t *testing.T) {
	// Port of coretests cmp.rs test_int_totalord.
	expectCmp(t, "5.cmp(&10)", 5, 10, Ordering_Less)
	expectCmp(t, "10.cmp(&5)", 10, 5, Ordering_Greater)
	expectCmp(t, "5.cmp(&5)", 5, 5, Ordering_Equal)
	expectCmp(t, "(-5).cmp(&12)", -5, 12, Ordering_Less)
	expectCmp(t, "12.cmp(&-5)", 12, -5, Ordering_Greater)
}

func TestCmp_test_bool_totalord(t *testing.T) {
	// Port of coretests cmp.rs test_bool_totalord.
	expectCmp(t, "true.cmp(&false)", true, false, Ordering_Greater)
	expectCmp(t, "false.cmp(&true)", false, true, Ordering_Less)
	expectCmp(t, "true.cmp(&true)", true, true, Ordering_Equal)
	expectCmp(t, "false.cmp(&false)", false, false, Ordering_Equal)
}

func TestCmp_test_mut_int_totalord(t *testing.T) {
	// Port of coretests cmp.rs test_mut_int_totalord: a reference is ordered
	// by what it refers to.
	ref := func(value int) *int { return &value }
	expectCmp(t, "(&mut 5).cmp(&&mut 10)", ref(5), ref(10), Ordering_Less)
	expectCmp(t, "(&mut 10).cmp(&&mut 5)", ref(10), ref(5), Ordering_Greater)
	expectCmp(t, "(&mut 5).cmp(&&mut 5)", ref(5), ref(5), Ordering_Equal)
	expectCmp(t, "(&mut -5).cmp(&&mut 12)", ref(-5), ref(12), Ordering_Less)
	expectCmp(t, "(&mut 12).cmp(&&mut -5)", ref(12), ref(-5), Ordering_Greater)
}

// probeRanked is ordered by rank alone, so two values can be equal in order
// and still be told apart.
type probeRanked struct {
	rank int
	name string
}

func (r probeRanked) Compare(other probeRanked) int { return r.rank - other.rank }

func TestCmp_test_ord_max_min(t *testing.T) {
	// Port of coretests cmp.rs test_ord_max_min.
	if Max(1, 2) != 2 || Max(2, 1) != 2 || Min(1, 2) != 1 || Min(2, 1) != 1 || Max(1, 1) != 1 || Min(1, 1) != 1 {
		t.Fatal("min and max of integers")
	}
	// std::cmp::min returns the first argument when the two are equal, and
	// std::cmp::max the second.
	first, second := probeRanked{rank: 1, name: "first"}, probeRanked{rank: 1, name: "second"}
	if Min(first, second).name != "first" || Max(first, second).name != "second" {
		t.Fatal("min keeps the first of two equal values, max the second")
	}
	lower := probeRanked{rank: 0, name: "lower"}
	if Min(first, lower).name != "lower" || Max(lower, first).name != "first" || Max(first, lower).name != "first" {
		t.Fatal("min and max of ranked values")
	}
	if Min("b", "a") != "a" || Max("b", "a") != "b" || Min(2.5, 1.5) != 1.5 {
		t.Fatal("min and max of strings and floats")
	}
}

func TestCmp_test_ordering_reverse(t *testing.T) {
	// Port of coretests cmp.rs test_ordering_reverse.
	if Ordering_Less.Reverse() != Ordering_Greater || Ordering_Equal.Reverse() != Ordering_Equal ||
		Ordering_Greater.Reverse() != Ordering_Less {
		t.Fatal("Ordering::reverse")
	}
}

func TestCmp_test_ordering_order(t *testing.T) {
	// Port of coretests cmp.rs test_ordering_order.
	if !(Cmp(Ordering_Less, Ordering_Equal) < 0) {
		t.Fatal("Less < Equal")
	}
	if OrdCmp(Ordering_Greater, Ordering_Less) != Ordering_Greater {
		t.Fatal("Greater.cmp(&Less) == Greater")
	}
}

func TestCmp_test_ordering_then(t *testing.T) {
	// Port of coretests cmp.rs test_ordering_then and test_ordering_then_with.
	all := []Ordering{Ordering_Less, Ordering_Equal, Ordering_Greater}
	for _, first := range all {
		for _, second := range all {
			want := first
			if first == Ordering_Equal {
				want = second
			}
			if got := first.Then(second); got != want {
				t.Errorf("%d.then(%d) = %d, want %d", first, second, got, want)
			}
			called := false
			got := first.ThenWith(func() Ordering { called = true; return second })
			if got != want {
				t.Errorf("%d.then_with(|| %d) = %d, want %d", first, second, got, want)
			}
			if called != (first == Ordering_Equal) {
				t.Errorf("%d.then_with called its closure: %v", first, called)
			}
		}
	}
}

func TestCmp_ordering_const(t *testing.T) {
	// Port of coretests cmp.rs ordering_const.
	const ordering = Ordering_Greater
	if ordering.Reverse() != Ordering_Less || Ordering_Equal.Then(ordering) != Ordering_Greater {
		t.Fatal("Ordering methods on a constant")
	}
}

func TestCmpOrderingIsAnEnum(t *testing.T) {
	cases := []struct {
		order                  Ordering
		eq, ne, lt, gt, le, ge bool
	}{
		{Ordering_Less, false, true, true, false, true, false},
		{Ordering_Equal, true, false, false, false, true, true},
		{Ordering_Greater, false, true, false, true, false, true},
	}
	for _, c := range cases {
		o := c.order
		if o.IsEq() != c.eq || o.IsNe() != c.ne || o.IsLt() != c.lt || o.IsGt() != c.gt || o.IsLe() != c.le || o.IsGe() != c.ge {
			t.Errorf("the is_* methods of %d", o)
		}
	}
	// `Less = -1, Equal = 0, Greater = 1`, as `ordering as i8` gives them.
	if int8(Ordering_Less) != -1 || int8(Ordering_Equal) != 0 || int8(Ordering_Greater) != 1 {
		t.Fatal("the discriminants of Ordering")
	}
	for name, order := range map[string]Ordering{"Less": Ordering_Less, "Equal": Ordering_Equal, "Greater": Ordering_Greater} {
		if variant := VariantOf(order); variant == nil || variant.RustName != name {
			t.Errorf("Ordering %d is not registered as %s", order, name)
		}
	}
	if OrderingOf(-7) != Ordering_Less || OrderingOf(0) != Ordering_Equal || OrderingOf(math.MaxInt) != Ordering_Greater ||
		OrderingOf(math.MinInt) != Ordering_Less {
		t.Fatal("OrderingOf keeps the sign")
	}
	if !Eq(Ordering_Less, Ordering_Less) || Eq(Ordering_Less, Ordering_Greater) {
		t.Fatal("Ordering equality")
	}
}

// probeInt and probeRevInt are Int and RevInt of coretests cmp.rs.
type probeInt struct{ F0 int }

func (i *probeInt) Compare(other *probeInt) int {
	return Cmp(i.F0, other.F0)
}

type probeRevInt struct{ F0 int }

func (i *probeRevInt) Compare(other *probeRevInt) int {
	return Cmp(other.F0, i.F0)
}

func TestCmp_cmp_default(t *testing.T) {
	// Port of the PartialOrd half of coretests cmp.rs cmp_default.
	if !(Cmp(probeInt{2}, probeInt{1}) > 0) || !(Cmp(probeInt{2}, probeInt{1}) >= 0) || !(Cmp(probeInt{1}, probeInt{1}) >= 0) ||
		!(Cmp(probeInt{1}, probeInt{2}) < 0) || !(Cmp(probeInt{1}, probeInt{2}) <= 0) || !(Cmp(probeInt{1}, probeInt{1}) <= 0) {
		t.Fatal("Int orders as its number")
	}
	if !(Cmp(probeRevInt{2}, probeRevInt{1}) < 0) || !(Cmp(probeRevInt{2}, probeRevInt{1}) <= 0) ||
		!(Cmp(probeRevInt{1}, probeRevInt{1}) <= 0) || !(Cmp(probeRevInt{1}, probeRevInt{2}) > 0) ||
		!(Cmp(probeRevInt{1}, probeRevInt{2}) >= 0) || !(Cmp(probeRevInt{1}, probeRevInt{1}) >= 0) {
		t.Fatal("RevInt orders the other way round")
	}
}

func TestCmpNumbersStringsAndEnums(t *testing.T) {
	type level int8
	type sessionId string
	expectCmp(t, "u8 above the sign bit", uint8(200), uint8(100), Ordering_Greater)
	expectCmp(t, "i8", int8(-1), int8(1), Ordering_Less)
	expectCmp(t, "u64 above the sign bit", uint64(1<<63), uint64(1), Ordering_Greater)
	expectCmp(t, "i64", int64(math.MinInt64), int64(math.MaxInt64), Ordering_Less)
	expectCmp(t, "newtype over i8", level(-3), level(2), Ordering_Less)
	expectCmp(t, "newtype over String", sessionId("a"), sessionId("b"), Ordering_Less)
	expectCmp(t, "char", 'a', 'é', Ordering_Less)
	expectCmp(t, "data-free enum", probeColor_Red, probeColor_Blue, Ordering_Less)
	expectCmp(t, "data-free enum, equal", probeColor_Green, probeColor_Green, Ordering_Equal)
	expectCmp(t, "unit", Unit{}, Unit{}, Ordering_Equal)

	// Strings order by their bytes, which for UTF-8 is by code point.
	expectCmp(t, "capital before small", "Zebra", "apple", Ordering_Less)
	expectCmp(t, "prefix", "ab", "abc", Ordering_Less)
	expectCmp(t, "empty", "", "a", Ordering_Less)
	expectCmp(t, "accented after z", "é", "z", Ordering_Greater)
	expectCmp(t, "astral after the BMP", "\U00010000", "￿", Ordering_Greater)
	expectCmp(t, "equal", "héllo", "héllo", Ordering_Equal)

	expectCmp(t, "floats", 1.5, 2.5, Ordering_Less)
	expectCmp(t, "negative zero", math.Copysign(0, -1), 0.0, Ordering_Equal)
	expectCmp(t, "infinities", math.Inf(-1), math.Inf(1), Ordering_Less)
	expectCmp(t, "f32", float32(2), float32(1), Ordering_Greater)
}

func TestCmpStructsTuplesSlicesOptionsAndResults(t *testing.T) {
	type version struct {
		major, minor int
		tag          string
		parts        []int
	}
	expectCmp(t, "first field decides", version{major: 1, minor: 9}, version{major: 2, minor: 0}, Ordering_Less)
	expectCmp(t, "second field decides", version{major: 1, minor: 9}, version{major: 1, minor: 10}, Ordering_Less)
	expectCmp(t, "a later private field decides", version{1, 1, "a", nil}, version{1, 1, "b", nil}, Ordering_Less)
	expectCmp(t, "a slice field decides", version{1, 1, "a", []int{1, 2}}, version{1, 1, "a", []int{1, 3}}, Ordering_Less)
	expectCmp(t, "equal", version{1, 1, "a", []int{1}}, version{1, 1, "a", []int{1}}, Ordering_Equal)

	expectCmp(t, "tuple, first", T2(1, "z"), T2(2, "a"), Ordering_Less)
	expectCmp(t, "tuple, second", T2(1, "a"), T2(1, "b"), Ordering_Less)
	expectCmp(t, "triple", T3(1, "a", false), T3(1, "a", true), Ordering_Less)

	expectCmp(t, "slices, element", []int{1, 2, 3}, []int{1, 3}, Ordering_Less)
	expectCmp(t, "slices, a prefix is less", []int{1, 2}, []int{1, 2, 0}, Ordering_Less)
	expectCmp(t, "slices, nil and empty", []int(nil), []int{}, Ordering_Equal)
	expectCmp(t, "slices, empty first", []string{}, []string{""}, Ordering_Less)
	expectCmp(t, "bytes", []byte("abc"), []byte("abd"), Ordering_Less)
	expectCmp(t, "bytes, prefix", []byte("ab"), []byte("abc"), Ordering_Less)
	expectCmp(t, "bytes above the sign bit", []byte{0x80}, []byte{0x7f}, Ordering_Greater)
	expectCmp(t, "signed bytes", []int8{-1}, []int8{1}, Ordering_Less)
	expectCmp(t, "nested slices", [][]int{{1}, {2}}, [][]int{{1}, {2, 0}}, Ordering_Less)
	expectCmp(t, "arrays", [3]int{1, 2, 3}, [3]int{1, 2, 4}, Ordering_Less)

	expectCmp(t, "None before Some", None[int](), Some(math.MinInt), Ordering_Less)
	expectCmp(t, "None and None", None[string](), None[string](), Ordering_Equal)
	expectCmp(t, "Some by value", Some("a"), Some("b"), Ordering_Less)
	expectCmp(t, "nested options", Some(None[int]()), Some(Some(0)), Ordering_Less)
	expectCmp(t, "option of a slice", Some([]int{1}), Some([]int{1, 1}), Ordering_Less)

	failure := &probeFault_Timeout{seconds: 1}
	expectCmp(t, "Ok before Err", Ok(100), Err[int](failure), Ordering_Less)
	expectCmp(t, "Ok by value", Ok(1), Ok(2), Ordering_Less)
	expectCmp(t, "Err by error", Err[int](&probeFault_Timeout{seconds: 1}), Err[int](&probeFault_Timeout{seconds: 2}), Ordering_Less)
	expectCmp(t, "Err by variant", Err[int](&probeFault_Timeout{seconds: 9}), Err[int](&probeFault_Refused{}), Ordering_Less)

	var missing *int
	one := 1
	expectCmp(t, "a nil pointer sorts first", missing, &one, Ordering_Less)
	expectCmp(t, "nil pointers", missing, missing, Ordering_Equal)
	expectCmp(t, "boxes", &probeBox{values: []int{1}}, &probeBox{values: []int{2}}, Ordering_Less)
}

// probeFault is an error enum.
type probeFault interface {
	error
	isProbeFault()
}

type probeFault_Timeout struct{ seconds int }
type probeFault_Refused struct{}

func (*probeFault_Timeout) isProbeFault() {}
func (*probeFault_Refused) isProbeFault() {}
func (*probeFault_Timeout) Error() string { return "timed out" }
func (*probeFault_Refused) Error() string { return "refused" }

var _ = RegisterEnum[probeFault]("Fault",
	Variant[probeFault_Timeout]("Timeout", ShapeStruct),
	Variant[probeFault_Refused]("Refused", ShapeUnit),
)

func TestCmpEnumsOrderByVariantThenFields(t *testing.T) {
	rect := func(width uint32, tags ...string) probeShape { return &probeShape_Rect{Width: width, Tags: tags} }
	expectCmp[probeShape](t, "earlier variant", &probeShape_Empty{}, &probeShape_Circle{F0: -100}, Ordering_Less)
	expectCmp[probeShape](t, "later variant, whatever it holds", &probeShape_Group{}, rect(1000), Ordering_Greater)
	expectCmp[probeShape](t, "same variant, first field", rect(1, "z"), rect(2, "a"), Ordering_Less)
	expectCmp[probeShape](t, "same variant, second field", rect(1, "a"), rect(1, "b"), Ordering_Less)
	expectCmp[probeShape](t, "same unit variant", &probeShape_Empty{}, &probeShape_Empty{}, Ordering_Equal)
	expectCmp[probeShape](t, "nested", &probeShape_Boxed{F0: rect(1)}, &probeShape_Boxed{F0: &probeShape_Group{}}, Ordering_Less)
	expectCmp[probeShape](t, "groups are slices",
		&probeShape_Group{F0: []probeShape{rect(1)}}, &probeShape_Group{F0: []probeShape{rect(1), &probeShape_Empty{}}}, Ordering_Less)
	expectCmp(t, "variants in an any", any(&probeShape_Empty{}), any(rect(1)), Ordering_Less)
	expectCmp(t, "values in an any", any("a"), any("b"), Ordering_Less)
	expectCmp(t, "nil interface first", probeShape(nil), probeShape(&probeShape_Empty{}), Ordering_Less)

	// Two types that are not variants of one registered enum have no order.
	defer func() {
		recovered := recover()
		if recovered == nil {
			t.Fatal("ordering unrelated types held in one interface must panic")
		}
		message, _ := recovered.(string)
		if !strings.Contains(message, "not variants of one registered enum") {
			t.Fatalf("panic = %v", recovered)
		}
	}()
	Cmp[probeToken](&probeToken_Word{}, &probeToken_Pair{})
}

func TestCmpPartialOrderOfFloats(t *testing.T) {
	nan := math.NaN()
	type sample struct {
		id    int
		value float64
		rest  string
	}
	partial := func(order Ordering) Option[Ordering] { return Some(order) }
	none := None[Ordering]()
	check := func(name string, got, want Option[Ordering]) {
		t.Helper()
		if got != want {
			t.Errorf("%s: partial_cmp = %+v, want %+v", name, got, want)
		}
	}
	check("floats", PartialCmp(1.0, 2.0), partial(Ordering_Less))
	check("NaN and a number", PartialCmp(nan, 1.0), none)
	check("a number and NaN", PartialCmp(1.0, nan), none)
	check("NaN and NaN", PartialCmp(nan, nan), none)
	check("f32 NaN", PartialCmp(float32(nan), float32(0)), none)
	check("a struct decided before the NaN", PartialCmp(sample{1, nan, "a"}, sample{2, nan, "a"}), partial(Ordering_Less))
	check("a struct stopped by the NaN", PartialCmp(sample{1, nan, "a"}, sample{1, nan, "b"}), none)
	check("a struct without NaN", PartialCmp(sample{1, 1, "a"}, sample{1, 1, "b"}), partial(Ordering_Less))
	check("a slice stopped by the NaN", PartialCmp([]float64{nan, 1}, []float64{nan, 2}), none)
	check("a slice decided before the NaN", PartialCmp([]float64{1, nan}, []float64{2, nan}), partial(Ordering_Less))
	check("an option", PartialCmp(Some(nan), Some(1.0)), none)
	check("an option decided by its variant", PartialCmp(None[float64](), Some(nan)), partial(Ordering_Less))
	check("a tuple", PartialCmp(T2(nan, 1), T2(nan, 2)), none)
	check("an enum", PartialCmp[probeShape](&probeShape_Circle{F0: nan}, &probeShape_Circle{F0: 1}), none)
	check("an enum decided by its variant", PartialCmp[probeShape](&probeShape_Empty{}, &probeShape_Circle{F0: nan}), partial(Ordering_Less))
	check("Reverse", PartialCmp(Reverse[float64]{nan}, Reverse[float64]{1}), none)
	check("integers", PartialCmp(1, 2), partial(Ordering_Less))
	check("strings", PartialCmp("b", "a"), partial(Ordering_Greater))

	// Cmp answers 0 where partial_cmp answers None, at any depth.
	if Cmp(nan, 1.0) != 0 || Cmp(1.0, nan) != 0 || Cmp(nan, nan) != 0 {
		t.Error("Cmp of NaN is 0")
	}
	if Cmp([]float64{nan, 1}, []float64{nan, 2}) != 0 || Cmp(sample{1, nan, "a"}, sample{1, nan, "b"}) != 0 {
		t.Error("Cmp of values that cannot be ordered is 0, whatever follows the NaN")
	}
	// Ord::min and Ord::max with `other < self` false: min keeps self, max
	// takes other.
	if got := Min(1.0, nan); got != 1.0 {
		t.Errorf("Min(1.0, NaN) = %v", got)
	}
	if got := Max(1.0, nan); !math.IsNaN(got) {
		t.Errorf("Max(1.0, NaN) = %v", got)
	}
}

func TestCmpReverse(t *testing.T) {
	expectCmp(t, "Reverse", Reverse[int]{1}, Reverse[int]{2}, Ordering_Greater)
	expectCmp(t, "Reverse, equal", Reverse[string]{"a"}, Reverse[string]{"a"}, Ordering_Equal)
	expectCmp(t, "Reverse of a tuple", Reverse[Tuple2[int, string]]{T2(1, "a")}, Reverse[Tuple2[int, string]]{T2(1, "b")}, Ordering_Greater)
	expectCmp(t, "Reverse of Reverse", Reverse[Reverse[int]]{Reverse[int]{1}}, Reverse[Reverse[int]]{Reverse[int]{2}}, Ordering_Less)

	// `sort_by_key(|row| (Reverse(row.age), row.pid))`, as mj-worker does.
	type row struct{ age, pid int }
	rows := []row{{age: 1, pid: 3}, {age: 5, pid: 2}, {age: 5, pid: 1}, {age: 3, pid: 9}}
	key := func(r row) Tuple2[Reverse[int], int] { return T2(Reverse[int]{r.age}, r.pid) }
	slices.SortStableFunc(rows, func(a, b row) int { return Cmp(key(a), key(b)) })
	want := []row{{5, 1}, {5, 2}, {3, 9}, {1, 3}}
	if !slices.Equal(rows, want) {
		t.Fatalf("sorted by (Reverse(age), pid) = %v, want %v", rows, want)
	}
}

// probeWeight has a Compare that answers with any negative or positive
// number, and one that answers with an Ordering.
type probeWeight struct{ grams int }

func (w probeWeight) Compare(other probeWeight) int { return (w.grams - other.grams) * 1000 }

type probePriority struct{ level int }

func (p *probePriority) Compare(other *probePriority) Ordering {
	return OrdCmp(other.level, p.level)
}

// probeCard is an enum with a hand-written Ord: aces high, whatever the
// variant order says.
type probeCard interface {
	isProbeCard()
	Compare(other probeCard) int
	strength() int
}

type probeCard_Ace struct{}
type probeCard_Number struct{ F0 int }

func (*probeCard_Ace) isProbeCard()                     {}
func (*probeCard_Number) isProbeCard()                  {}
func (*probeCard_Ace) strength() int                    { return 14 }
func (n *probeCard_Number) strength() int               { return n.F0 }
func (a *probeCard_Ace) Compare(other probeCard) int    { return a.strength() - other.strength() }
func (n *probeCard_Number) Compare(other probeCard) int { return n.strength() - other.strength() }

func TestCmpUsesHandWrittenCompareWhereverTheValueIs(t *testing.T) {
	expectCmp(t, "the sign of an int result", probeWeight{5}, probeWeight{7}, Ordering_Less)
	expectCmp(t, "a Compare returning an Ordering", probePriority{1}, probePriority{2}, Ordering_Greater)
	expectCmp(t, "in a slice", []probeWeight{{1}, {5}}, []probeWeight{{1}, {7}}, Ordering_Less)
	expectCmp(t, "behind a pointer", &probePriority{1}, &probePriority{2}, Ordering_Greater)
	expectCmp(t, "in an option", Some(probeWeight{9}), Some(probeWeight{7}), Ordering_Greater)
	expectCmp(t, "in a struct", struct {
		name string
		w    probeWeight
	}{"a", probeWeight{1}}, struct {
		name string
		w    probeWeight
	}{"a", probeWeight{2}}, Ordering_Less)
	expectCmp(t, "in an any", any(probeWeight{1}), any(probeWeight{2}), Ordering_Less)
	expectCmp[probeCard](t, "an enum's own order across variants", &probeCard_Ace{}, &probeCard_Number{F0: 10}, Ordering_Greater)
	expectCmp[probeCard](t, "an enum's own order within a variant", &probeCard_Number{F0: 2}, &probeCard_Number{F0: 10}, Ordering_Less)
	expectCmp(t, "an enum's own order in a slice", []probeCard{&probeCard_Number{F0: 3}}, []probeCard{&probeCard_Ace{}}, Ordering_Less)
}

func TestCmpCollectionsAndMaps(t *testing.T) {
	tree := func(pairs ...any) probeTreeMap[string, int] {
		var m probeTreeMap[string, int]
		for i := 0; i < len(pairs); i += 2 {
			m.Insert(pairs[i].(string), pairs[i+1].(int))
		}
		return m
	}
	// BTreeMap orders as the sequence of its (key, value) pairs.
	expectCmp(t, "map, first key", tree("a", 9), tree("b", 0), Ordering_Less)
	expectCmp(t, "map, value", tree("a", 1, "b", 1), tree("a", 1, "b", 2), Ordering_Less)
	expectCmp(t, "map, prefix", tree("a", 1), tree("a", 1, "b", 0), Ordering_Less)
	expectCmp(t, "map, equal", tree("a", 1, "b", 2), tree("b", 2, "a", 1), Ordering_Equal)
	expectCmp(t, "map, key before value", tree("a", 9, "c", 0), tree("a", 9, "b", 5), Ordering_Greater)

	deque := func(items ...int) probeDeque[int] {
		var d probeDeque[int]
		for _, item := range items {
			d.PushBack(item)
		}
		return d
	}
	expectCmp(t, "deque", deque(1, 2, 3), deque(1, 3), Ordering_Less)
	expectCmp(t, "deque, prefix", deque(1), deque(1, 0), Ordering_Less)
	expectCmp(t, "deque, order matters", deque(2, 1), deque(1, 2), Ordering_Greater)

	set := func(items ...int) probeTreeSet[int] {
		var s probeTreeSet[int]
		for _, item := range items {
			s.Insert(item)
		}
		return s
	}
	expectCmp(t, "set", set(3, 1), set(1, 4), Ordering_Less)
	expectCmp(t, "set, equal", set(3, 1, 2), set(1, 2, 3), Ordering_Equal)

	// Rust has no order for these; Cmp gives one that does not depend on
	// the order of the entries.
	expectCmp(t, "Go maps", map[string]int{"a": 1, "b": 2}, map[string]int{"b": 3, "a": 1}, Ordering_Less)
	expectCmp(t, "Go maps, equal", map[string]int{"a": 1, "b": 2}, map[string]int{"b": 2, "a": 1}, Ordering_Equal)
	hash := func(items ...string) probeHashSet[string] {
		var s probeHashSet[string]
		for _, item := range items {
			s.Insert(item)
		}
		return s
	}
	expectCmp(t, "hash sets", hash("b", "a"), hash("a", "c"), Ordering_Less)
	expectCmp(t, "hash sets, equal", hash("b", "a"), hash("a", "b"), Ordering_Equal)
	var first, second probeHashMap[string, int]
	first.Insert("b", 1)
	first.Insert("a", 1)
	second.Insert("a", 1)
	second.Insert("b", 2)
	expectCmp(t, "hash maps", first, second, Ordering_Less)
}

func TestCmpSortsWithTheStandardLibrary(t *testing.T) {
	// Cmp has the signature slices.SortFunc asks for.
	leaves := []probeLeaf{newProbeLeaf("b"), newProbeLeaf("a"), newProbeLeaf("c")}
	slices.SortFunc(leaves, Cmp[probeLeaf])
	if leaves[0].Label != "a" || leaves[1].Label != "b" || leaves[2].Label != "c" {
		t.Fatalf("sorted = %v", []string{leaves[0].Label, leaves[1].Label, leaves[2].Label})
	}
	shapes := []probeShape{&probeShape_Group{}, &probeShape_Circle{F0: 2}, &probeShape_Empty{}, &probeShape_Circle{F0: 1}}
	slices.SortFunc(shapes, Cmp[probeShape])
	if _, ok := shapes[0].(*probeShape_Empty); !ok {
		t.Fatal("the first variant sorts first")
	}
	if shapes[1].(*probeShape_Circle).F0 != 1 || shapes[2].(*probeShape_Circle).F0 != 2 {
		t.Fatal("one variant sorts by its fields")
	}
	if _, ok := shapes[3].(*probeShape_Group); !ok {
		t.Fatal("the last variant sorts last")
	}
}
