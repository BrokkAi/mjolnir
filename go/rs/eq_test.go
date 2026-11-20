package rs

import (
	"errors"
	"math"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestEqScalarsAndStrings(t *testing.T) {
	type sessionId string
	type level int8
	nan := math.NaN()
	cases := []struct {
		name string
		got  bool
		want bool
	}{
		{"int equal", Eq(3, 3), true},
		{"int differs", Eq(3, 4), false},
		{"uint64 high bit", Eq(uint64(1<<63), uint64(1<<63)), true},
		{"string equal", Eq("héllo", "héllo"), true},
		{"string differs", Eq("a", "b"), false},
		{"bool", Eq(true, false), false},
		{"newtype over a string", Eq(sessionId("a"), sessionId("a")), true},
		{"newtype over an integer", Eq(level(-1), level(1)), false},
		{"data-free enum equal", Eq(probeColor_Blue, probeColor_Blue), true},
		{"data-free enum differs", Eq(probeColor_Red, probeColor_Blue), false},
		{"float equal", Eq(1.5, 1.5), true},
		{"NaN is not equal to itself", Eq(nan, nan), false},
		{"float32 NaN is not equal to itself", Eq(float32(nan), float32(nan)), false},
		{"negative zero equals zero", Eq(math.Copysign(0, -1), 0.0), true},
		{"char", Eq('é', 'é'), true},
		{"unit", Eq(Unit{}, Unit{}), true},
		{"complex", Eq(complex(1, 2), complex(1, 2)), true},
		{"complex differs", Eq(complex64(complex(1, 2)), complex64(complex(1, 3))), false},
	}
	for _, c := range cases {
		if c.got != c.want {
			t.Errorf("%s: got %v, want %v", c.name, c.got, c.want)
		}
	}
}

func TestEqSlicesArraysAndTuples(t *testing.T) {
	nan := math.NaN()
	shared := []float64{1, nan}
	cases := []struct {
		name string
		got  bool
		want bool
	}{
		{"equal slices", Eq([]int{1, 2}, []int{1, 2}), true},
		{"different element", Eq([]int{1, 2}, []int{1, 3}), false},
		{"prefix", Eq([]int{1, 2}, []int{1}), false},
		{"a nil slice equals an empty one", Eq([]int(nil), []int{}), true},
		{"an empty slice equals a nil one", Eq([]string{}, []string(nil)), true},
		{"nil is not a slice of one", Eq([]int(nil), []int{0}), false},
		{"bytes", Eq([]byte("abc"), []byte("abc")), true},
		{"bytes differ", Eq([]byte("abc"), []byte("abd")), false},
		{"nested", Eq([][]string{{"a"}, nil}, [][]string{{"a"}, {}}), true},
		{"nested differs", Eq([][]string{{"a"}, {"b"}}, [][]string{{"a"}, {"c"}}), false},
		{"a slice holding NaN is not equal to itself", Eq(shared, shared), false},
		{"wide integers", Eq([]int64{1 << 40, -1}, []int64{1 << 40, -1}), true},
		{"wide integers differ in the high half", Eq([]int64{1 << 40}, []int64{1 << 41}), false},
		{"arrays", Eq([3]int{1, 2, 3}, [3]int{1, 2, 3}), true},
		{"arrays differ", Eq([3]int{1, 2, 3}, [3]int{1, 2, 4}), false},
		{"array of slices", Eq([2][]int{{1}, nil}, [2][]int{{1}, {}}), true},
		{"tuples", Eq(T2("a", []int{1}), T2("a", []int{1})), true},
		{"tuples differ in the second field", Eq(T3(1, "a", 2.0), T3(1, "b", 2.0)), false},
		{"slice of units", Eq([]Unit{{}, {}}, []Unit{{}, {}}), true},
		{"slice of units of another length", Eq([]Unit{{}}, []Unit{{}, {}}), false},
	}
	for _, c := range cases {
		if c.got != c.want {
			t.Errorf("%s: got %v, want %v", c.name, c.got, c.want)
		}
	}
}

func TestEqStructsComparePrivateFieldsAndFollowPointers(t *testing.T) {
	left, right := newProbeLeaf("leaf"), newProbeLeaf("leaf")
	if !Eq(left, right) {
		t.Fatal("two structs built alike are equal, though their pointers differ")
	}
	right.box.next.values[0] = 99
	if Eq(left, right) {
		t.Fatal("a difference behind two pointers in a private field went unseen")
	}
	right = newProbeLeaf("leaf")
	right.notes = append(right.notes, "more")
	if Eq(left, right) {
		t.Fatal("a difference in a private slice went unseen")
	}
	right = newProbeLeaf("leaf")
	right.box.next = nil
	if Eq(left, right) || Eq(right, left) {
		t.Fatal("a nil pointer is not equal to a pointer")
	}

	if !Eq(&left, &left) || !Eq(&left, ClonePtr(&left)) {
		t.Fatal("pointers are compared by what they point to")
	}
	var none *probeLeaf
	if !Eq(none, none) || Eq(none, &left) {
		t.Fatal("nil pointers")
	}

	type padded struct {
		_ int
		A string
		_ [2]float64
	}
	if !Eq(padded{A: "a"}, padded{A: "a"}) {
		t.Fatal("blank fields take no part")
	}
}

func TestEqPointersToNaN(t *testing.T) {
	// A Box or an Arc of a value that is not equal to itself is not equal to
	// itself either (alloctests arc.rs float_nan_ne).
	type reading struct {
		label string
		value float64
	}
	withNaN := &reading{label: "r", value: math.NaN()}
	if Eq(withNaN, withNaN) {
		t.Fatal("a pointer to a NaN-holding value must not be taken as equal to itself")
	}
	whole := &reading{label: "r", value: 1}
	if !Eq(whole, whole) {
		t.Fatal("a pointer is equal to itself when its value is")
	}
	if shapeOf(reflect.TypeFor[reading]()).isReflexive() {
		t.Fatal("a struct with a float is not provably reflexive")
	}
	if !shapeOf(reflect.TypeFor[probeLeaf]()).isReflexive() {
		t.Fatal("a struct of strings, slices and boxes is reflexive")
	}
	if !shapeOf(reflect.TypeFor[probeNode]()).isReflexive() {
		t.Fatal("a recursive struct without floats is reflexive")
	}
	if shapeOf(reflect.TypeFor[probeShape]()).isReflexive() {
		t.Fatal("an enum with an f64 variant is not reflexive")
	}
	if shapeOf(reflect.TypeFor[probeToken]()).isReflexive() {
		t.Fatal("an unregistered interface cannot be proven reflexive")
	}
	if !shapeOf(reflect.TypeFor[probeColor]()).isReflexive() {
		t.Fatal("a data-free enum is reflexive")
	}
}

func TestEqEnums(t *testing.T) {
	rect := func(width uint32, tags ...string) probeShape { return &probeShape_Rect{Width: width, Tags: tags} }
	cases := []struct {
		name string
		a, b probeShape
		want bool
	}{
		{"same unit variant", &probeShape_Empty{}, &probeShape_Empty{}, true},
		{"same variant, equal fields", rect(1, "a"), rect(1, "a"), true},
		{"same variant, other field", rect(1, "a"), rect(2, "a"), false},
		{"same variant, other nested slice", rect(1, "a"), rect(1, "b"), false},
		{"different variants", &probeShape_Empty{}, &probeShape_Circle{}, false},
		{"float payload", &probeShape_Circle{F0: 1}, &probeShape_Circle{F0: 1}, true},
		{"NaN payload", &probeShape_Circle{F0: math.NaN()}, &probeShape_Circle{F0: math.NaN()}, false},
		{"nested", &probeShape_Group{F0: []probeShape{rect(1), &probeShape_Boxed{F0: &probeShape_Empty{}}}},
			&probeShape_Group{F0: []probeShape{rect(1), &probeShape_Boxed{F0: &probeShape_Empty{}}}}, true},
		{"nested differs", &probeShape_Group{F0: []probeShape{rect(1), &probeShape_Boxed{F0: &probeShape_Empty{}}}},
			&probeShape_Group{F0: []probeShape{rect(1), &probeShape_Boxed{F0: &probeShape_Circle{}}}}, false},
		{"nil and nil", nil, nil, true},
		{"nil and a variant", nil, &probeShape_Empty{}, false},
	}
	for _, c := range cases {
		if got := Eq(c.a, c.b); got != c.want {
			t.Errorf("%s: got %v, want %v", c.name, got, c.want)
		}
		if got := Eq(c.b, c.a); got != c.want {
			t.Errorf("%s, swapped: got %v, want %v", c.name, got, c.want)
		}
	}

	word := func(text string) probeToken { return &probeToken_Word{text: []byte(text)} }
	if !Eq[probeToken](&probeToken_Pair{left: word("a"), right: word("b")}, &probeToken_Pair{left: word("a"), right: word("b")}) {
		t.Error("an unregistered sealed enum compares by variant and fields")
	}
	if Eq[probeToken](&probeToken_Pair{left: word("a"), right: word("b")}, &probeToken_Pair{left: word("a"), right: word("c")}) {
		t.Error("an unregistered sealed enum: a nested difference went unseen")
	}

	// Values in an `any`: the same type and equal values.
	anyCases := []struct {
		name string
		a, b any
		want bool
	}{
		{"strings", "a", "a", true},
		{"an int64 is not a float64", int64(1), float64(1), false},
		{"an int64 is not a uint64", int64(1), uint64(1), false},
		{"floats", 1.5, 1.5, true},
		{"NaN", math.NaN(), math.NaN(), false},
		{"slices", []any{"a", int64(1), nil}, []any{"a", int64(1), nil}, true},
		{"slices differ", []any{"a", []any{1.0}}, []any{"a", []any{2.0}}, false},
		{"structs by value", newProbeLeaf("x"), newProbeLeaf("x"), true},
		{"structs by value differ", newProbeLeaf("x"), newProbeLeaf("y"), false},
		{"variants", rect(1, "a"), rect(1, "a"), true},
		{"bool", true, true, true},
		{"nil", nil, nil, true},
		{"nil and a value", nil, "", false},
	}
	for _, c := range anyCases {
		if got := Eq(c.a, c.b); got != c.want {
			t.Errorf("any %s: got %v, want %v", c.name, got, c.want)
		}
	}
}

func TestEqOptionsAndResults(t *testing.T) {
	failure := errors.New("boom")
	cases := []struct {
		name string
		got  bool
		want bool
	}{
		{"None and None", Eq(None[int](), None[int]()), true},
		{"Some and Some", Eq(Some([]int{1}), Some([]int{1})), true},
		{"Some and another Some", Eq(Some("a"), Some("b")), false},
		{"Some and None", Eq(Some(0), None[int]()), false},
		{"None and Some of the zero value", Eq(None[string](), Some("")), false},
		{"Some(NaN)", Eq(Some(math.NaN()), Some(math.NaN())), false},
		{"None of floats", Eq(None[float64](), None[float64]()), true},
		{"nested options", Eq(Some(None[int]()), Some(None[int]())), true},
		{"nested options differ", Eq(Some(Some(1)), Some(None[int]())), false},
		{"option of a box", Eq(Some(&probeBox{values: []int{1}}), Some(&probeBox{values: []int{1}})), true},
		{"Ok and Ok", Eq(Ok([]int{1}), Ok([]int{1})), true},
		{"Ok and another Ok", Eq(Ok(1), Ok(2)), false},
		{"Ok and Err", Eq(Ok(0), Err[int](failure)), false},
		{"Err and the same Err", Eq(Err[int](failure), Err[int](failure)), true},
		{"Err ignores the unused value", Eq(Result[int]{Value: 1, Err: failure}, Result[int]{Value: 2, Err: failure}), true},
	}
	for _, c := range cases {
		if c.got != c.want {
			t.Errorf("%s: got %v, want %v", c.name, c.got, c.want)
		}
	}
}

func TestEqGoMaps(t *testing.T) {
	cases := []struct {
		name string
		a, b map[string][]int
		want bool
	}{
		{"equal", map[string][]int{"a": {1}, "b": nil}, map[string][]int{"b": {}, "a": {1}}, true},
		{"nil and empty", nil, map[string][]int{}, true},
		{"other value", map[string][]int{"a": {1}}, map[string][]int{"a": {2}}, false},
		{"other key", map[string][]int{"a": {1}}, map[string][]int{"b": {1}}, false},
		{"other length", map[string][]int{"a": {1}}, map[string][]int{"a": {1}, "b": {2}}, false},
	}
	for _, c := range cases {
		if got := Eq(c.a, c.b); got != c.want {
			t.Errorf("%s: got %v, want %v", c.name, got, c.want)
		}
	}
	withNaN := map[string]float64{"a": math.NaN()}
	if Eq(withNaN, withNaN) {
		t.Error("a map holding NaN is not equal to itself")
	}
	whole := map[string]int{"a": 1}
	if !Eq(whole, whole) {
		t.Error("a map of integers is equal to itself")
	}
}

func TestEqCollections(t *testing.T) {
	tree := func(pairs ...any) probeTreeMap[string, []int] {
		var m probeTreeMap[string, []int]
		for i := 0; i < len(pairs); i += 2 {
			m.Insert(pairs[i].(string), pairs[i+1].([]int))
		}
		return m
	}
	if !Eq(tree("a", []int{1}, "b", []int{2}), tree("b", []int{2}, "a", []int{1})) {
		t.Error("ordered maps with the same entries are equal")
	}
	if Eq(tree("a", []int{1}), tree("a", []int{2})) || Eq(tree("a", []int{1}), tree("b", []int{1})) ||
		Eq(tree("a", []int{1}), tree("a", []int{1}, "b", []int(nil))) {
		t.Error("ordered maps: a difference went unseen")
	}
	if !Eq(tree(), tree()) {
		t.Error("empty maps are equal")
	}

	hash := func(pairs ...any) probeHashMap[string, probeLeaf] {
		var m probeHashMap[string, probeLeaf]
		for i := 0; i < len(pairs); i += 2 {
			m.Insert(pairs[i].(string), newProbeLeaf(pairs[i+1].(string)))
		}
		return m
	}
	if !Eq(hash("a", "x", "b", "y"), hash("b", "y", "a", "x")) {
		t.Error("the order of a hash map's entries means nothing")
	}
	if Eq(hash("a", "x", "b", "y"), hash("a", "x", "b", "z")) || Eq(hash("a", "x"), hash("c", "x")) {
		t.Error("hash maps: a difference went unseen")
	}

	// Keys that Go's == cannot index: enum values.
	shapeKeyed := func(widths ...uint32) probeHashMap[probeShape, int] {
		var m probeHashMap[probeShape, int]
		for _, width := range widths {
			m.Insert(&probeShape_Rect{Width: width}, int(width))
		}
		return m
	}
	if !Eq(shapeKeyed(1, 2, 3), shapeKeyed(3, 1, 2)) {
		t.Error("hash maps keyed by enum values compare by key, whatever the order")
	}
	if Eq(shapeKeyed(1, 2, 3), shapeKeyed(1, 2, 4)) {
		t.Error("hash maps keyed by enum values: another key went unseen")
	}

	deque := func(items ...string) probeDeque[string] {
		var d probeDeque[string]
		for _, item := range items {
			d.PushBack(item)
		}
		return d
	}
	if !Eq(deque("a", "b"), deque("a", "b")) || Eq(deque("a", "b"), deque("b", "a")) || Eq(deque("a"), deque("a", "a")) {
		t.Error("a deque compares element by element, in order")
	}

	treeSet := func(items ...int) probeTreeSet[int] {
		var s probeTreeSet[int]
		for _, item := range items {
			s.Insert(item)
		}
		return s
	}
	if !Eq(treeSet(3, 1, 2), treeSet(1, 2, 3)) || Eq(treeSet(1, 2), treeSet(1, 3)) {
		t.Error("ordered sets")
	}

	hashSet := func(items ...string) probeHashSet[string] {
		var s probeHashSet[string]
		for _, item := range items {
			s.Insert(item)
		}
		return s
	}
	if !Eq(hashSet("a", "b", "c"), hashSet("c", "a", "b")) || Eq(hashSet("a", "b"), hashSet("a", "c")) ||
		Eq(hashSet("a"), hashSet("a", "b")) {
		t.Error("the order of a hash set's elements means nothing")
	}
	shapeSet := func(widths ...uint32) probeHashSet[probeShape] {
		var s probeHashSet[probeShape]
		for _, width := range widths {
			s.Insert(&probeShape_Rect{Width: width})
		}
		return s
	}
	if !Eq(shapeSet(1, 2), shapeSet(2, 1)) || Eq(shapeSet(1, 2), shapeSet(1, 3)) {
		t.Error("hash sets of enum values compare by element, whatever the order")
	}

	// Collections nested in a struct, and a clone of them.
	type holder struct {
		byName probeTreeMap[string, []int]
		seen   probeHashSet[string]
		queue  probeDeque[probeShape]
	}
	original := holder{byName: tree("a", []int{1}), seen: hashSet("x")}
	original.queue.PushBack(&probeShape_Rect{Width: 1})
	if !Eq(original, Clone(original)) {
		t.Error("a struct of collections is equal to its clone")
	}
	other := Clone(original)
	other.queue.PushBack(&probeShape_Empty{})
	if Eq(original, other) {
		t.Error("a struct of collections: a longer deque went unseen")
	}
}

// probeSketchy is SketchyNum of coretests cmp.rs.
type probeSketchy struct{ num int }

// Our custom eq allows numbers which are near each other to be equal! :D
func (s *probeSketchy) Equal(other *probeSketchy) bool {
	difference := s.num - other.num
	return difference < 5 && difference > -5
}

// probeFool is Fool of coretests cmp.rs: equal exactly when the two differ.
type probeFool bool

func (f probeFool) Equal(other probeFool) bool { return f != other }

// probeMeta is an object whose equality ignores the order of its members, as
// jsonvalue.Object's does.
type probeMeta []probeMember

func (m probeMeta) Equal(other probeMeta) bool {
	if len(m) != len(other) {
		return false
	}
	for _, member := range m {
		found := false
		for _, candidate := range other {
			if candidate.Key == member.Key {
				found = Eq(candidate.Value, member.Value)
			}
		}
		if !found {
			return false
		}
	}
	return true
}

// probeUnit is an enum with a hand-written PartialEq that sets units aside:
// each variant carries the method, which takes the enum.
type probeUnit interface {
	isProbeUnit()
	Equal(other probeUnit) bool
	meters() float64
}

type probeUnit_Meters struct{ F0 float64 }
type probeUnit_Feet struct{ F0 float64 }

func (*probeUnit_Meters) isProbeUnit()                 {}
func (*probeUnit_Feet) isProbeUnit()                   {}
func (m *probeUnit_Meters) meters() float64            { return m.F0 }
func (f *probeUnit_Feet) meters() float64              { return f.F0 * 0.5 }
func (m *probeUnit_Meters) Equal(other probeUnit) bool { return m.meters() == other.meters() }
func (f *probeUnit_Feet) Equal(other probeUnit) bool   { return f.meters() == other.meters() }

func TestEq_test_user_defined_eq(t *testing.T) {
	// Port of coretests cmp.rs test_user_defined_eq.
	if !Eq(probeSketchy{num: 37}, probeSketchy{num: 34}) {
		t.Fatal("SketchyNum { num: 37 } == SketchyNum { num: 34 }")
	}
	if Eq(probeSketchy{num: 25}, probeSketchy{num: 57}) {
		t.Fatal("SketchyNum { num: 25 } != SketchyNum { num: 57 }")
	}
}

func TestEq_cmp_default(t *testing.T) {
	// Port of the PartialEq half of coretests cmp.rs cmp_default.
	if !Eq(probeFool(true), probeFool(false)) || !Eq(probeFool(false), probeFool(true)) {
		t.Fatal("Fool(true) == Fool(false)")
	}
	if Eq(probeFool(true), probeFool(true)) || Eq(probeFool(false), probeFool(false)) {
		t.Fatal("Fool(true) != Fool(true)")
	}
}

func TestEqUsesHandWrittenEqualWhereverTheValueIs(t *testing.T) {
	near, far := probeSketchy{num: 10}, probeSketchy{num: 12}
	type holder struct {
		direct probeSketchy
		boxed  *probeSketchy
		many   []probeSketchy
		maybe  Option[probeSketchy]
		held   any
		fool   probeFool
	}
	left := holder{direct: near, boxed: &near, many: []probeSketchy{near}, maybe: Some(near), held: near, fool: true}
	right := holder{direct: far, boxed: &far, many: []probeSketchy{far}, maybe: Some(far), held: far, fool: false}
	if !Eq(left, right) {
		t.Fatal("hand-written Equal methods were not used for nested values")
	}
	right.fool = true
	if Eq(left, right) {
		t.Fatal("Fool(true) != Fool(true) inside a struct")
	}
	if !Eq(&near, &far) {
		t.Fatal("a pointer to a type with Equal compares through it")
	}
	// Not reflexive by proof: the method decides even for one pointer.
	if !Eq(probeFool(true), probeFool(false)) || Eq(&left.fool, &left.fool) {
		t.Fatal("a hand-written Equal must be asked even about one and the same value")
	}

	meta := probeMeta{{Key: "a", Value: int64(1)}, {Key: "b", Value: []any{"x"}}}
	reordered := probeMeta{{Key: "b", Value: []any{"x"}}, {Key: "a", Value: int64(1)}}
	if !Eq(meta, reordered) || !Eq[any](meta, reordered) || !Eq([]any{meta}, []any{reordered}) {
		t.Fatal("a named slice with Equal compares through it, in an interface too")
	}

	var meters probeUnit = &probeUnit_Meters{F0: 1}
	var feet probeUnit = &probeUnit_Feet{F0: 2}
	if !Eq(meters, feet) || !Eq(feet, meters) || Eq(meters, probeUnit(&probeUnit_Feet{F0: 3})) {
		t.Fatal("an enum's hand-written Equal decides across variants")
	}
	if !Eq([]probeUnit{meters}, []probeUnit{feet}) {
		t.Fatal("an enum's hand-written Equal decides inside a slice")
	}

	// time.Time carries its own Equal: one instant in two zones.
	utc := time.Date(2026, 11, 20, 12, 0, 0, 0, time.UTC)
	elsewhere := utc.In(time.FixedZone("x", 3600))
	if !Eq(utc, elsewhere) || !Eq(struct{ at time.Time }{utc}, struct{ at time.Time }{elsewhere}) {
		t.Fatal("time.Time compares by its Equal")
	}
	if Eq(utc, utc.Add(time.Nanosecond)) {
		t.Fatal("different instants are not equal")
	}
}

func TestEqFuncsChannelsAndLocksCompareByIdentity(t *testing.T) {
	captured := 0
	first := func() int { captured++; return captured }
	second := func() int { captured++; return captured }
	type callbacks struct {
		run    func() int
		events chan int
	}
	events := make(chan int)
	if !Eq(callbacks{run: first, events: events}, callbacks{run: first, events: events}) {
		t.Error("the same func and channel are equal")
	}
	if Eq(callbacks{run: first, events: events}, callbacks{run: second, events: events}) {
		t.Error("two closures are different funcs")
	}
	if Eq(callbacks{run: first, events: events}, callbacks{run: first, events: make(chan int)}) {
		t.Error("two channels are different")
	}
	if !Eq(callbacks{}, callbacks{}) {
		t.Error("nil funcs and channels are equal")
	}

	one, two := &probeGuarded{items: []int{1}}, &probeGuarded{items: []int{1}}
	if !Eq(one, one) || Eq(one, two) {
		t.Error("pointers to a struct holding a lock compare by identity")
	}
	lockA, lockB := &sync.Mutex{}, &sync.Mutex{}
	if !Eq(lockA, lockA) || Eq(lockA, lockB) {
		t.Error("pointers to locks compare by identity")
	}
	backendA, backendB := &probeBackend{name: "b"}, &probeBackend{name: "b"}
	if !Eq[probeService](backendA, backendB) {
		t.Error("two values of one type held in an interface compare by value")
	}
}

func TestEqCellsAndReverse(t *testing.T) {
	// Port of coretests cell.rs smoketest_cell, as far as equality goes.
	x := CellNew(10)
	if !Eq(x, CellNew(10)) {
		t.Fatal("Cell::new(10) == Cell::new(10)")
	}
	x.Set(20)
	if !Eq(x, CellNew(20)) || Eq(x, CellNew(10)) {
		t.Fatal("a Cell compares by its value")
	}
	y := CellNew(T2(30, 40))
	if !Eq(y, CellNew(T2(30, 40))) {
		t.Fatal("a Cell of a tuple")
	}
	if !Eq(RefCellNew([]int{1}), RefCellNew([]int{1})) || Eq(RefCellNew([]int{1}), RefCellNew([]int{2})) {
		t.Fatal("a RefCell compares by its value")
	}
	if !Eq(Reverse[int]{F0: 1}, Reverse[int]{F0: 1}) || Eq(Reverse[string]{F0: "a"}, Reverse[string]{F0: "b"}) {
		t.Fatal("Reverse compares by its value")
	}
}

// probeFoldOption is the comparer option the tests register, as
// go/rs/rspath registers "path": strings that differ only in case are equal,
// and ordered as their lower case, which no byte comparison does.
const probeFoldOption = "rstest_fold"

func init() {
	RegisterTagComparer(probeFoldOption,
		func(a, b string) int { return strings.Compare(strings.ToLower(a), strings.ToLower(b)) },
		strings.EqualFold)
}

// probeFolded has the option on every shape it applies to.
type probeFolded struct {
	Name  string           `rs:"name,rstest_fold"`
	Maybe Option[string]   `rs:"maybe,rstest_fold"`
	List  []string         `rs:"list,rstest_fold"`
	Lists Option[[]string] `rs:"lists,rstest_fold"`
	Plain string           `rs:"plain"`
}

// probeFoldKey could key a Go map, but Go's == does not answer what Eq does.
type probeFoldKey struct {
	Path string `rs:"path,rstest_fold"`
}

type probeFoldedName string

// probeFoldRoute is an enum whose variant holds tagged tuple fields, one of
// them an Arc<[T]> as well.
type probeFoldRoute interface{ isProbeFoldRoute() }

type probeFoldRoute_Dir struct {
	F0 probeFoldedName   `rs:",rstest_fold"`
	F1 []probeFoldedName `rs:",arc,rstest_fold"`
}

func (*probeFoldRoute_Dir) isProbeFoldRoute() {}

type probeUnregistered struct {
	Where string `rs:"where,rstest_unregistered"`
}

type probeMisshapenPair struct {
	Pair Tuple2[string, string] `rs:"pair,rstest_fold"`
}

type probeMisshapenMap struct {
	Paths map[string]string `rs:"paths,rstest_fold"`
}

type probeMisshapenBox struct {
	Path *string `rs:"path,rstest_fold"`
}

type probeTwoOptions struct {
	Path string `rs:"path,rstest_fold,rstest_other"`
}

func TestEqAndCmpUseTheComparerATagNames(t *testing.T) {
	folded := func(name string) probeFolded { return probeFolded{Name: name} }
	if !Eq(folded("Src/Main"), folded("src/main")) || Eq(probeFolded{Plain: "A"}, probeFolded{Plain: "a"}) {
		t.Fatal("a tagged string is compared by its comparer, an untagged one by its bytes")
	}
	// Bytes would put "B" before "a".
	if Cmp(folded("B"), folded("a")) <= 0 || Cmp(folded("a"), folded("B")) >= 0 ||
		PartialCmp(folded("A"), folded("a")) != Some(Ordering_Equal) {
		t.Fatal("a tagged string is ordered by its comparer")
	}
	cases := []struct {
		name  string
		a, b  probeFolded
		equal bool
		order int
	}{
		{"None before Some", probeFolded{}, probeFolded{Maybe: Some("")}, false, -1},
		{"Some by the comparer", probeFolded{Maybe: Some("X")}, probeFolded{Maybe: Some("x")}, true, 0},
		{"Some ordered by the comparer", probeFolded{Maybe: Some("b")}, probeFolded{Maybe: Some("A")}, false, 1},
		{"elements by the comparer", probeFolded{List: []string{"A", "b"}}, probeFolded{List: []string{"a", "B"}}, true, 0},
		{"nil and empty", probeFolded{List: nil}, probeFolded{List: []string{}}, true, 0},
		{"a prefix first", probeFolded{List: []string{"a"}}, probeFolded{List: []string{"A", "b"}}, false, -1},
		{"the first difference", probeFolded{List: []string{"B"}}, probeFolded{List: []string{"a", "z"}}, false, 1},
		{"None before Some of nothing", probeFolded{}, probeFolded{Lists: Some([]string(nil))}, false, -1},
		{"Some of a slice by the comparer", probeFolded{Lists: Some([]string{"X"})}, probeFolded{Lists: Some([]string{"x"})}, true, 0},
		{"an earlier tagged field first", probeFolded{Name: "a", Plain: "z"}, probeFolded{Name: "B", Plain: "a"}, false, -1},
		{"a later untagged field by its bytes", probeFolded{Name: "a", Plain: "B"}, probeFolded{Name: "A", Plain: "a"}, false, -1},
	}
	for _, c := range cases {
		if got := Eq(c.a, c.b); got != c.equal {
			t.Errorf("%s: Eq = %v", c.name, got)
		}
		if got, swapped := Cmp(c.a, c.b), Cmp(c.b, c.a); got != c.order || swapped != -c.order {
			t.Errorf("%s: Cmp = %d, swapped %d, want %d", c.name, got, swapped, c.order)
		}
	}

	// In an enum variant, and beside `arc`, which is not a comparer option.
	dir := &probeFoldRoute_Dir{F0: "Home", F1: []probeFoldedName{"X"}}
	if !Eq[probeFoldRoute](dir, &probeFoldRoute_Dir{F0: "HOME", F1: []probeFoldedName{"x"}}) ||
		Cmp[probeFoldRoute](dir, &probeFoldRoute_Dir{F0: "home", F1: []probeFoldedName{"y"}}) >= 0 {
		t.Fatal("a tagged tuple field of an enum variant is compared by its comparer")
	}
	if cloned := Clone(probeFoldRoute(dir)).(*probeFoldRoute_Dir); cloned == dir || !Eq(*cloned, *dir) {
		t.Fatal("a variant with tagged fields is cloned as any other")
	}

	// A key with a tagged field is found by its comparer, not by Go's ==.
	var left, right probeHashMap[probeFoldKey, int]
	left.Insert(probeFoldKey{Path: "A"}, 1)
	right.Insert(probeFoldKey{Path: "a"}, 1)
	var leftSet, rightSet probeHashSet[probeFoldKey]
	leftSet.Insert(probeFoldKey{Path: "A"})
	rightSet.Insert(probeFoldKey{Path: "a"})
	if !Eq(left, right) || !Eq(leftSet, rightSet) {
		t.Fatal("hash keys with a tagged field")
	}
	if EqIsGoEqual(reflect.TypeFor[probeFoldKey]()) || !EqIsGoEqual(reflect.TypeFor[struct{ Path string }]()) {
		t.Fatal("EqIsGoEqual and a tagged field")
	}

	// Clone ignores the option, even one nobody registered.
	original := probeFolded{Name: "N", List: []string{"a"}, Lists: Some([]string{"b"})}
	copied := Clone(original)
	copied.List[0] = "changed"
	(*copied.Lists.AsRef())[0] = "changed"
	if original.List[0] != "a" || original.Lists.Unwrap()[0] != "b" || copied.Name != "N" {
		t.Fatal("Clone copies tagged fields as any other")
	}
	if Clone(probeUnregistered{Where: "x"}).Where != "x" {
		t.Fatal("Clone of a field with an unregistered option")
	}
}

func TestEqAndCmpPanicOnATagTheyCannotHonour(t *testing.T) {
	expectPanic(t, `rs.Eq: the field Where of rs.probeUnregistered is tagged with the option "rstest_unregistered", `+
		`which nothing registered with rs.RegisterTagComparer`, func() { Eq(probeUnregistered{}, probeUnregistered{}) })
	expectPanic(t, `rs.Cmp: the field Where of rs.probeUnregistered`, func() { Cmp(probeUnregistered{}, probeUnregistered{}) })
	expectPanic(t, `the field Where of rs.probeUnregistered`, func() {
		Eq([]probeUnregistered{{Where: "a"}}, []probeUnregistered{{Where: "a"}})
	})
	for _, misshapen := range []struct {
		want    string
		compare func()
	}{
		{`rs.Eq: the field Pair of rs.probeMisshapenPair is tagged with the option "rstest_fold" but holds a rs.Tuple2`,
			func() { Eq(probeMisshapenPair{}, probeMisshapenPair{}) }},
		{`rs.Cmp: the field Paths of rs.probeMisshapenMap is tagged with the option "rstest_fold" but holds a map[string]string`,
			func() { Cmp(probeMisshapenMap{}, probeMisshapenMap{}) }},
		{`the field Path of rs.probeMisshapenBox is tagged with the option "rstest_fold" but holds a *string`,
			func() { Eq(probeMisshapenBox{}, probeMisshapenBox{}) }},
		{`the field Path of rs.probeTwoOptions names the comparer options rstest_fold, rstest_other`,
			func() { Eq(probeTwoOptions{}, probeTwoOptions{}) }},
	} {
		expectPanic(t, misshapen.want, misshapen.compare)
	}
	expectPanic(t, "compare this field by hand", func() { PartialCmp(probeMisshapenPair{}, probeMisshapenPair{}) })

	exact := func(a, b string) bool { return a == b }
	expectPanic(t, "a comparer is already registered for the tag option rstest_fold", func() {
		RegisterTagComparer(probeFoldOption, strings.Compare, exact)
	})
	expectPanic(t, `"arc" cannot be the tag option of a comparer`, func() { RegisterTagComparer("arc", strings.Compare, exact) })
	expectPanic(t, `"" cannot be the tag option of a comparer`, func() { RegisterTagComparer("", strings.Compare, exact) })
	expectPanic(t, "needs both functions", func() { RegisterTagComparer("rstest_half", nil, exact) })
	// The refused registration left the option unregistered.
	type half struct {
		Path string `rs:"path,rstest_half"`
	}
	expectPanic(t, `option "rstest_half", which nothing registered`, func() { Eq(half{}, half{}) })
}
