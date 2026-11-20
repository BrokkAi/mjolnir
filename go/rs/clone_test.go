package rs

import (
	"bytes"
	"cmp"
	"errors"
	"math"
	"reflect"
	"regexp"
	"slices"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// The fakes below stand in for the collections of go/rs/collections, which
// describe themselves to the structural operations through ReflectMap and
// ReflectSeq. clone_test.go, eq_test.go, cmp_test.go and default_test.go
// share them.

// probeTreeMap keeps its entries in key order, as a BTreeMap does.
type probeTreeMap[K cmp.Ordered, V any] struct {
	keys   []K
	values []*V
}

func (m *probeTreeMap[K, V]) Insert(key K, value V) {
	at, found := slices.BinarySearch(m.keys, key)
	if found {
		m.values[at] = &value
		return
	}
	m.keys = slices.Insert(m.keys, at, key)
	m.values = slices.Insert(m.values, at, &value)
}

func (m *probeTreeMap[K, V]) Get(key K) *V {
	if at, found := slices.BinarySearch(m.keys, key); found {
		return m.values[at]
	}
	return nil
}

func (m *probeTreeMap[K, V]) ReflectLen() int                  { return len(m.keys) }
func (m *probeTreeMap[K, V]) ReflectKeyType() reflect.Type     { return reflect.TypeFor[K]() }
func (m *probeTreeMap[K, V]) ReflectValueType() reflect.Type   { return reflect.TypeFor[V]() }
func (m *probeTreeMap[K, V]) ReflectInsert(key any, value any) { m.Insert(key.(K), value.(V)) }
func (m *probeTreeMap[K, V]) ReflectSorted() bool              { return true }
func (m *probeTreeMap[K, V]) ReflectEntries(yield func(key any, value any) bool) {
	for at, key := range m.keys {
		if !yield(key, m.values[at]) {
			return
		}
	}
}

// probeHashMap keeps its entries in insertion order and says that the order
// means nothing, as a HashMap and an IndexMap do.
type probeHashMap[K comparable, V any] struct {
	keys   []K
	values []*V
}

func (m *probeHashMap[K, V]) Insert(key K, value V) {
	if at := slices.Index(m.keys, key); at >= 0 {
		m.values[at] = &value
		return
	}
	m.keys = append(m.keys, key)
	m.values = append(m.values, &value)
}

func (m *probeHashMap[K, V]) Get(key K) *V {
	if at := slices.Index(m.keys, key); at >= 0 {
		return m.values[at]
	}
	return nil
}

func (m *probeHashMap[K, V]) ReflectLen() int                  { return len(m.keys) }
func (m *probeHashMap[K, V]) ReflectKeyType() reflect.Type     { return reflect.TypeFor[K]() }
func (m *probeHashMap[K, V]) ReflectValueType() reflect.Type   { return reflect.TypeFor[V]() }
func (m *probeHashMap[K, V]) ReflectInsert(key any, value any) { m.Insert(key.(K), value.(V)) }
func (m *probeHashMap[K, V]) ReflectSorted() bool              { return false }
func (m *probeHashMap[K, V]) ReflectEntries(yield func(key any, value any) bool) {
	for at, key := range m.keys {
		if !yield(key, m.values[at]) {
			return
		}
	}
}

// probeDeque is a sequence, as a VecDeque is.
type probeDeque[T any] struct {
	items []*T
}

func (d *probeDeque[T]) PushBack(item T)               { d.items = append(d.items, &item) }
func (d *probeDeque[T]) ReflectLen() int               { return len(d.items) }
func (d *probeDeque[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }
func (d *probeDeque[T]) ReflectPush(element any)       { d.PushBack(element.(T)) }
func (d *probeDeque[T]) ReflectIsSet() bool            { return false }
func (d *probeDeque[T]) ReflectSorted() bool           { return false }
func (d *probeDeque[T]) ReflectElements(yield func(element any) bool) {
	for _, item := range d.items {
		if !yield(item) {
			return
		}
	}
}

// probeTreeSet keeps its elements in order, as a BTreeSet does.
type probeTreeSet[T cmp.Ordered] struct {
	items []*T
}

func (s *probeTreeSet[T]) Insert(item T) {
	at, found := slices.BinarySearchFunc(s.items, item, func(have *T, want T) int { return cmp.Compare(*have, want) })
	if !found {
		s.items = slices.Insert(s.items, at, &item)
	}
}

func (s *probeTreeSet[T]) ReflectLen() int               { return len(s.items) }
func (s *probeTreeSet[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }
func (s *probeTreeSet[T]) ReflectPush(element any)       { s.Insert(element.(T)) }
func (s *probeTreeSet[T]) ReflectIsSet() bool            { return true }
func (s *probeTreeSet[T]) ReflectSorted() bool           { return true }
func (s *probeTreeSet[T]) ReflectElements(yield func(element any) bool) {
	for _, item := range s.items {
		if !yield(item) {
			return
		}
	}
}

// probeHashSet keeps its elements in insertion order and says that the
// order means nothing, as a HashSet does.
type probeHashSet[T comparable] struct {
	items []*T
}

func (s *probeHashSet[T]) Insert(item T) {
	for _, have := range s.items {
		if *have == item {
			return
		}
	}
	s.items = append(s.items, &item)
}

func (s *probeHashSet[T]) ReflectLen() int               { return len(s.items) }
func (s *probeHashSet[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }
func (s *probeHashSet[T]) ReflectPush(element any)       { s.Insert(element.(T)) }
func (s *probeHashSet[T]) ReflectIsSet() bool            { return true }
func (s *probeHashSet[T]) ReflectSorted() bool           { return false }
func (s *probeHashSet[T]) ReflectElements(yield func(element any) bool) {
	for _, item := range s.items {
		if !yield(item) {
			return
		}
	}
}

var (
	_ ReflectMap = (*probeTreeMap[string, int])(nil)
	_ ReflectMap = (*probeHashMap[string, int])(nil)
	_ ReflectSeq = (*probeDeque[int])(nil)
	_ ReflectSeq = (*probeTreeSet[int])(nil)
	_ ReflectSeq = (*probeHashSet[int])(nil)
)

// probeColor is a data-free enum.
type probeColor uint8

const (
	probeColor_Red probeColor = iota
	probeColor_Green
	probeColor_Blue
)

var _ = RegisterEnum[probeColor]("Color",
	Const(probeColor_Red, "Red"),
	Const(probeColor_Green, "Green"),
	Const(probeColor_Blue, "Blue"),
)

// probeShape is an enum whose variants carry data, registered.
//
//	enum Shape { Empty, Circle(f64), Rect { width: u32, tags: Vec<String> }, Group(Vec<Shape>), Boxed(Box<Shape>) }
type probeShape interface{ isProbeShape() }

type probeShape_Empty struct{}

type probeShape_Circle struct{ F0 float64 }

type probeShape_Rect struct {
	Width uint32   `rs:"width"`
	Tags  []string `rs:"tags"`
}

type probeShape_Group struct{ F0 []probeShape }

type probeShape_Boxed struct{ F0 probeShape }

func (*probeShape_Empty) isProbeShape()  {}
func (*probeShape_Circle) isProbeShape() {}
func (*probeShape_Rect) isProbeShape()   {}
func (*probeShape_Group) isProbeShape()  {}
func (*probeShape_Boxed) isProbeShape()  {}

var _ = RegisterEnum[probeShape]("Shape",
	Variant[probeShape_Empty]("Empty", ShapeUnit),
	Variant[probeShape_Circle]("Circle", ShapeTuple),
	Variant[probeShape_Rect]("Rect", ShapeStruct),
	Variant[probeShape_Group]("Group", ShapeTuple),
	Variant[probeShape_Boxed]("Boxed", ShapeTuple),
)

// probeToken is a sealed enum that nobody registered: the unexported method
// alone says that its variants are owned values.
type probeToken interface{ isProbeToken() }

type probeToken_Word struct{ text []byte }

type probeToken_Pair struct{ left, right probeToken }

func (*probeToken_Word) isProbeToken() {}
func (*probeToken_Pair) isProbeToken() {}

// probeService is a trait: values of it are trait objects, not enum
// variants.
type probeService interface{ Name() string }

type probeBackend struct {
	name  string
	calls []string
}

func (b *probeBackend) Name() string { return b.name }

// probeLeaf is a small struct with a boxed part, private fields included.
type probeLeaf struct {
	Label string `rs:"label"`
	notes []string
	box   *probeBox
}

type probeBox struct {
	values []int
	next   *probeBox
}

func newProbeLeaf(label string) probeLeaf {
	return probeLeaf{
		Label: label,
		notes: []string{label + "-note"},
		box:   &probeBox{values: []int{1, 2}, next: &probeBox{values: []int{3}}},
	}
}

func TestCloneCopiesPlainValuesAsTheyAre(t *testing.T) {
	type point struct {
		X, Y int32
		name string
		on   bool
		kind probeColor
		grid [2][2]float64
	}
	original := point{X: 1, Y: -2, name: "p", on: true, kind: probeColor_Blue, grid: [2][2]float64{{1, 2}, {3, 4}}}
	if got := Clone(original); got != original {
		t.Fatalf("Clone(%+v) = %+v", original, got)
	}
	if got := Clone("héllo"); got != "héllo" {
		t.Fatalf("Clone of a string = %q", got)
	}
	if got := Clone(uint64(1<<63 + 5)); got != 1<<63+5 {
		t.Fatalf("Clone of a uint64 = %d", got)
	}
	if got := Clone(probeColor_Green); got != probeColor_Green {
		t.Fatalf("Clone of a data-free enum = %d", got)
	}
	if got := Clone(complex64(complex(1, 2))); got != complex(1, 2) {
		t.Fatalf("Clone of a complex64 = %v", got)
	}
	if got := Clone(Unit{}); got != (Unit{}) {
		t.Fatalf("Clone of the unit = %v", got)
	}
	if !shapeOf(reflect.TypeFor[point]()).plain {
		t.Fatal("a struct of numbers and strings should be copied in one piece")
	}
}

func TestCloneSlices(t *testing.T) {
	original := make([]int, 3, 10)
	copy(original, []int{1, 2, 3})
	cloned := Clone(original)
	if !slices.Equal(cloned, original) {
		t.Fatalf("Clone = %v", cloned)
	}
	if cap(cloned) != len(cloned) {
		t.Fatalf("a cloned Vec has capacity %d for length %d; Vec::clone allocates exactly the length", cap(cloned), len(cloned))
	}
	cloned[0] = 99
	if original[0] != 1 {
		t.Fatal("the clone shares the backing array")
	}

	if got := Clone([]int(nil)); got != nil {
		t.Fatalf("a nil slice cloned to %#v", got)
	}
	if got := Clone([]int{}); got == nil || len(got) != 0 {
		t.Fatalf("an empty slice cloned to %#v", got)
	}

	nested := [][]string{{"a", "b"}, nil, {}}
	nestedClone := Clone(nested)
	if !reflect.DeepEqual(nested, nestedClone) {
		t.Fatalf("nested clone = %#v", nestedClone)
	}
	nestedClone[0][0] = "changed"
	if nested[0][0] != "a" {
		t.Fatal("the inner slices are shared")
	}

	blob := []byte("bytes")
	blobClone := Clone(blob)
	blobClone[0] = 'B'
	if string(blob) != "bytes" {
		t.Fatal("a []byte outside an arc field must be copied")
	}
}

func TestCloneArraysMapsOptionsResultsAndTuples(t *testing.T) {
	array := [2][]int{{1}, {2, 3}}
	arrayClone := Clone(array)
	arrayClone[1][0] = 9
	if array[1][0] != 2 {
		t.Fatal("array elements are shared")
	}

	goMap := map[string][]int{"a": {1}, "b": nil}
	mapClone := Clone(goMap)
	if !reflect.DeepEqual(goMap, mapClone) {
		t.Fatalf("map clone = %#v", mapClone)
	}
	mapClone["a"][0] = 9
	mapClone["c"] = []int{7}
	if goMap["a"][0] != 1 || len(goMap) != 2 {
		t.Fatal("the map or its values are shared")
	}
	if got := Clone(map[string]int(nil)); got != nil {
		t.Fatalf("a nil map cloned to %#v", got)
	}
	plainMap := map[int]string{1: "one"}
	plainClone := Clone(plainMap)
	plainClone[2] = "two"
	if len(plainMap) != 1 {
		t.Fatal("a map of plain values is shared")
	}

	some := Some([]string{"x"})
	someClone := Clone(some)
	(*someClone.AsRef())[0] = "y"
	if some.Unwrap()[0] != "x" {
		t.Fatal("the value of an Option is shared")
	}
	if none := Clone(None[[]string]()); none.IsSome() {
		t.Fatal("None cloned to Some")
	}

	ok := Ok([]int{1, 2})
	okClone := Clone(ok)
	okClone.Value[0] = 9
	if ok.Value[0] != 1 || okClone.Err != nil {
		t.Fatal("the value of a Result is shared")
	}
	failure := errors.New("boom")
	if errClone := Clone(Err[int](failure)); errClone.Err != failure {
		t.Fatal("an error that is not an enum is shared, as an Arc<dyn Error> is")
	}

	tuple := T3([]int{1}, "two", &probeBox{values: []int{3}})
	tupleClone := Clone(tuple)
	tupleClone.F0[0] = 9
	tupleClone.F2.values[0] = 9
	if tuple.F0[0] != 1 || tuple.F2.values[0] != 3 || tupleClone.F1 != "two" || tuple.F2 == tupleClone.F2 {
		t.Fatal("tuple fields are shared")
	}
}

func TestCloneBoxesAreCopiedDeeplyWithPrivateFields(t *testing.T) {
	original := newProbeLeaf("leaf")
	cloned := Clone(original)
	if !reflect.DeepEqual(original, cloned) {
		t.Fatalf("clone differs: %#v", cloned)
	}
	if cloned.box == original.box || cloned.box.next == original.box.next {
		t.Fatal("a Box must be a new allocation at every level")
	}
	cloned.notes[0] = "changed"
	cloned.box.values[0] = 99
	cloned.box.next.values[0] = 99
	if original.notes[0] != "leaf-note" || original.box.values[0] != 1 || original.box.next.values[0] != 3 {
		t.Fatalf("private fields are shared: %#v", original)
	}

	boxed := &original
	boxedClone := Clone(boxed)
	if boxedClone == boxed || boxedClone.box == boxed.box {
		t.Fatal("Clone of a pointer is Box::clone: a new allocation")
	}
	viaPtr := ClonePtr(boxed)
	if viaPtr == boxed || viaPtr.box == boxed.box || viaPtr.Label != "leaf" {
		t.Fatal("ClonePtr must copy deeply")
	}
	var missing *probeLeaf
	if Clone(missing) != nil {
		t.Fatal("a nil pointer clones to nil")
	}
	double := &boxed
	doubleClone := Clone(double)
	if doubleClone == double || *doubleClone == *double || (*doubleClone).box == (*double).box {
		t.Fatal("a pointer to a pointer is a Box<Box<T>>")
	}
}

// probeShared exercises every form an `arc` tag covers.
type probeShared struct {
	Owned      *probeBox                         `rs:"owned"`
	Shared     *probeBox                         `rs:"shared,arc"`
	Maybe      Option[*probeBox]                 `rs:"maybe,arc"`
	MaybeOwned Option[*probeBox]                 `rs:"maybe_owned"`
	List       []*probeBox                       `rs:"list,arc"`
	OwnedList  []*probeBox                       `rs:"owned_list"`
	Bytes      []byte                            `rs:"bytes,arc"`
	Items      []probeLeaf                       `rs:"items,arc"`
	Chunks     [][]byte                          `rs:"chunks,arc"`
	Names      []string                          `rs:"names,arc"`
	ByName     map[string]*probeBox              `rs:"by_name,arc"`
	Tree       probeTreeMap[string, *probeBox]   `rs:"tree,arc"`
	Queue      probeDeque[*probeBox]             `rs:"queue,arc"`
	Pair       Tuple2[*probeBox, probeLeaf]      `rs:"pair,arc"`
	Trait      probeService                      `rs:"trait,arc"`
	Variant    probeShape                        `rs:"variant,arc"`
	Nested     []Option[*probeBox]               `rs:"nested,arc"`
	Grid       [2]*probeBox                      `rs:"grid,arc"`
	OwnedTree  probeTreeMap[string, *probeBox]   `rs:"owned_tree"`
	hidden     *probeBox                         `rs:"hidden,arc"`
	hiddenOwn  *probeBox                         `rs:"hidden_own"`
	Lists      probeHashMap[string, []*probeBox] `rs:"lists,arc"`
}

func TestCloneSharesWhatAnArcTagCovers(t *testing.T) {
	box := func(value int) *probeBox { return &probeBox{values: []int{value}} }
	original := probeShared{
		Owned:      box(1),
		Shared:     box(2),
		Maybe:      Some(box(3)),
		MaybeOwned: Some(box(4)),
		List:       []*probeBox{box(5), box(6)},
		OwnedList:  []*probeBox{box(7)},
		Bytes:      []byte("shared bytes"),
		Items:      []probeLeaf{newProbeLeaf("item")},
		Chunks:     [][]byte{[]byte("one"), []byte("two")},
		Names:      []string{"a", "b"},
		ByName:     map[string]*probeBox{"k": box(8)},
		Pair:       T2(box(9), newProbeLeaf("pair")),
		Trait:      &probeBackend{name: "backend"},
		Variant:    &probeShape_Rect{Width: 3, Tags: []string{"t"}},
		Nested:     []Option[*probeBox]{Some(box(10)), None[*probeBox]()},
		Grid:       [2]*probeBox{box(11), box(12)},
		hidden:     box(13),
		hiddenOwn:  box(14),
	}
	original.Tree.Insert("t", box(15))
	original.Queue.PushBack(box(16))
	original.OwnedTree.Insert("o", box(17))
	original.Lists.Insert("l", []*probeBox{box(18)})

	cloned := Clone(original)
	if !reflect.DeepEqual(original, cloned) {
		t.Fatalf("the clone is not equal to the original:\n%#v\n%#v", original, cloned)
	}

	// Box pointers are new allocations.
	if cloned.Owned == original.Owned {
		t.Error("owned: a Box was shared")
	}
	if cloned.MaybeOwned.Unwrap() == original.MaybeOwned.Unwrap() {
		t.Error("maybe_owned: an Option<Box<T>> was shared")
	}
	if cloned.OwnedList[0] == original.OwnedList[0] {
		t.Error("owned_list: a Vec<Box<T>> shared its boxes")
	}
	if cloned.hiddenOwn == original.hiddenOwn {
		t.Error("hidden_own: a private Box was shared")
	}
	if *cloned.OwnedTree.Get("o") == *original.OwnedTree.Get("o") {
		t.Error("owned_tree: a BTreeMap<String, Box<T>> shared its boxes")
	}

	// Arc pointers are the same pointers.
	if cloned.Shared != original.Shared {
		t.Error("shared: an Arc<T> was copied")
	}
	if cloned.hidden != original.hidden {
		t.Error("hidden: a private Arc<T> was copied")
	}
	if cloned.Maybe.Unwrap() != original.Maybe.Unwrap() {
		t.Error("maybe: an Option<Arc<T>> was copied")
	}
	if cloned.Trait != original.Trait {
		t.Error("trait: an Arc<dyn Trait> was copied")
	}
	if cloned.Variant != original.Variant {
		t.Error("variant: an Arc<Enum> was copied")
	}
	if cloned.Grid != original.Grid {
		t.Error("grid: an [Arc<T>; 2] copied its pointers")
	}

	// A container of Arcs is a new container holding the same pointers.
	if &cloned.List[0] == &original.List[0] {
		t.Error("list: a Vec<Arc<T>> shared its buffer")
	}
	if cloned.List[0] != original.List[0] || cloned.List[1] != original.List[1] {
		t.Error("list: a Vec<Arc<T>> copied its pointers")
	}
	if &cloned.Nested[0] == &original.Nested[0] || cloned.Nested[0].Unwrap() != original.Nested[0].Unwrap() {
		t.Error("nested: a Vec<Option<Arc<T>>> must be a new Vec of the same pointers")
	}
	cloned.ByName["new"] = box(0)
	if len(original.ByName) != 1 || cloned.ByName["k"] != original.ByName["k"] {
		t.Error("by_name: a HashMap<String, Arc<T>> must be a new map of the same pointers")
	}
	cloned.Tree.Insert("new", box(0))
	if original.Tree.ReflectLen() != 1 || *cloned.Tree.Get("t") != *original.Tree.Get("t") {
		t.Error("tree: a BTreeMap<String, Arc<T>> must be a new map of the same pointers")
	}
	cloned.Queue.PushBack(box(0))
	if original.Queue.ReflectLen() != 1 || *cloned.Queue.items[0] != *original.Queue.items[0] {
		t.Error("queue: a VecDeque<Arc<T>> must be a new deque of the same pointers")
	}
	clonedLists, originalLists := *cloned.Lists.Get("l"), *original.Lists.Get("l")
	if &clonedLists[0] == &originalLists[0] || clonedLists[0] != originalLists[0] {
		t.Error("lists: a HashMap<String, Vec<Arc<T>>> must copy the map and the Vecs and share the pointers")
	}
	if &cloned.Names[0] == &original.Names[0] {
		t.Error("names: a Vec<Arc<str>> shared its buffer")
	}
	if &cloned.Chunks[0] == &original.Chunks[0] || &cloned.Chunks[0][0] != &original.Chunks[0][0] {
		t.Error("chunks: a Vec<Arc<[u8]>> must be a new Vec of the same slices")
	}

	// A slice with nothing shareable below it is the Arc<[T]> itself.
	if &cloned.Bytes[0] != &original.Bytes[0] {
		t.Error("bytes: an Arc<[u8]> was copied")
	}
	if &cloned.Items[0] != &original.Items[0] {
		t.Error("items: an Arc<[Item]> was copied")
	}

	// A tuple is reached through; a struct inside it is cloned on its own.
	if cloned.Pair.F0 != original.Pair.F0 {
		t.Error("pair.0: an Arc<T> in a tuple was copied")
	}
	if cloned.Pair.F1.box == original.Pair.F1.box {
		t.Error("pair.1: a struct beside an Arc in a tuple must be cloned by its own rules")
	}
}

func TestCloneEnums(t *testing.T) {
	inner := &probeShape_Rect{Width: 2, Tags: []string{"a", "b"}}
	var shape probeShape = &probeShape_Group{F0: []probeShape{
		inner,
		&probeShape_Boxed{F0: &probeShape_Circle{F0: 1.5}},
		&probeShape_Empty{},
	}}
	cloned := Clone(shape)
	if !reflect.DeepEqual(shape, cloned) {
		t.Fatalf("clone differs: %#v", cloned)
	}
	group := cloned.(*probeShape_Group)
	if group == shape.(*probeShape_Group) || group.F0[0] == probeShape(inner) {
		t.Fatal("an enum value is owned: its variant must be a new allocation")
	}
	group.F0[0].(*probeShape_Rect).Tags[0] = "changed"
	group.F0[1].(*probeShape_Boxed).F0.(*probeShape_Circle).F0 = 9
	if inner.Tags[0] != "a" || shape.(*probeShape_Group).F0[1].(*probeShape_Boxed).F0.(*probeShape_Circle).F0 != 1.5 {
		t.Fatal("nested variants are shared")
	}

	// A registered variant is recognised wherever it is held.
	var anyShape any = inner
	anyClone := Clone(anyShape)
	if anyClone == anyShape || anyClone.(*probeShape_Rect).Width != 2 {
		t.Fatal("a registered variant in an `any` must be copied")
	}
	held := struct{ Value any }{Value: inner}
	if Clone(held).Value == held.Value {
		t.Fatal("a registered variant in an `any` field must be copied")
	}

	// A sealed interface is an enum even when nobody registered it.
	var token probeToken = &probeToken_Pair{left: &probeToken_Word{text: []byte("l")}, right: &probeToken_Word{text: []byte("r")}}
	tokenClone := Clone(token).(*probeToken_Pair)
	tokenClone.left.(*probeToken_Word).text[0] = 'X'
	if token.(*probeToken_Pair).left.(*probeToken_Word).text[0] != 'l' {
		t.Fatal("the variants of a sealed interface are shared")
	}

	var none probeShape
	if Clone(none) != nil {
		t.Fatal("a nil interface clones to nil")
	}
	var typedNil probeShape = (*probeShape_Rect)(nil)
	if got := Clone(typedNil); got != typedNil {
		t.Fatal("an interface holding a nil pointer clones to itself")
	}
}

func TestCloneSharesTraitObjectsAndCopiesValuesInInterfaces(t *testing.T) {
	backend := &probeBackend{name: "b", calls: []string{"x"}}
	holder := struct {
		Service probeService
		Any     any
		Text    any
		List    any
		Leaf    any
		Fn      func() int
		Events  chan int
	}{
		Service: backend,
		Any:     backend,
		Text:    "text",
		List:    []any{"a", 1.5, []any{int64(2)}, nil},
		Leaf:    newProbeLeaf("held"),
		Fn:      func() int { return 7 },
		Events:  make(chan int, 1),
	}
	cloned := Clone(holder)
	if cloned.Service != holder.Service || cloned.Any != holder.Any {
		t.Fatal("a trait object is an Arc<dyn Trait>: it must be shared")
	}
	if cloned.Text != "text" || cloned.Fn() != 7 || cloned.Events != holder.Events {
		t.Fatal("strings, funcs and channels are copied as they are")
	}
	cloned.List.([]any)[2].([]any)[0] = int64(9)
	if holder.List.([]any)[2].([]any)[0] != int64(2) {
		t.Fatal("a slice held in an `any` must be copied deeply")
	}
	if !reflect.DeepEqual(cloned.List, []any{"a", 1.5, []any{int64(9)}, nil}) {
		t.Fatalf("cloned list = %#v", cloned.List)
	}
	clonedLeaf, heldLeaf := cloned.Leaf.(probeLeaf), holder.Leaf.(probeLeaf)
	if clonedLeaf.box == heldLeaf.box || clonedLeaf.Label != "held" {
		t.Fatal("a struct held by value in an `any` must be cloned as a struct")
	}
}

// probeCounted has a hand-written Clone on the value.
type probeCounted struct {
	id     int
	clones *int
}

func (c probeCounted) Clone() probeCounted {
	*c.clones++
	return probeCounted{id: c.id + 100, clones: c.clones}
}

// probeManual has a hand-written Clone on the pointer, as rule 2 writes it.
type probeManual struct {
	text  string
	cache []int
}

func (m *probeManual) Clone() probeManual {
	return probeManual{text: m.text + "+clone"}
}

// probeSender is a handle: its Clone returns the pointer to use.
type probeSender struct {
	channel *[]int
	senders *int
}

func (s *probeSender) Clone() *probeSender {
	*s.senders++
	return &probeSender{channel: s.channel, senders: s.senders}
}

// probeObject is a named slice with its own Clone, as jsonvalue.Object is.
type probeObject []probeMember

type probeMember struct {
	Key   string
	Value any
}

func (o probeObject) Clone() probeObject {
	out := make(probeObject, len(o), len(o)+4)
	copy(out, o)
	return out
}

// probeExpr is an enum with a hand-written Clone, which each variant carries
// because a Go interface cannot.
type probeExpr interface {
	isProbeExpr()
	Clone() probeExpr
}

type probeExpr_Lit struct{ value int }

func (*probeExpr_Lit) isProbeExpr() {}
func (e *probeExpr_Lit) Clone() probeExpr {
	return &probeExpr_Lit{value: e.value * 2}
}

func TestCloneUsesHandWrittenCloneMethods(t *testing.T) {
	clones := 0
	counted := probeCounted{id: 1, clones: &clones}
	if got := Clone(counted); got.id != 101 || clones != 1 {
		t.Fatalf("a Clone method on the value was not used: %+v after %d calls", got, clones)
	}

	type wrapper struct {
		first  probeCounted
		many   []probeCounted
		maybe  Option[probeCounted]
		boxed  *probeCounted
		shared []probeCounted `rs:"shared,arc"`
		manual probeManual
		ptr    *probeManual
	}
	clones = 0
	original := wrapper{
		first:  counted,
		many:   []probeCounted{counted, counted},
		maybe:  Some(counted),
		boxed:  &counted,
		shared: []probeCounted{counted},
		manual: probeManual{text: "m", cache: []int{1}},
		ptr:    &probeManual{text: "p", cache: []int{2}},
	}
	cloned := Clone(original)
	if clones != 6 {
		t.Fatalf("Clone ran %d times for six values", clones)
	}
	if cloned.first.id != 101 || cloned.many[1].id != 101 || cloned.maybe.Unwrap().id != 101 || cloned.shared[0].id != 101 {
		t.Fatalf("results of Clone were not stored: %+v", cloned)
	}
	if cloned.boxed == original.boxed || cloned.boxed.id != 101 {
		t.Fatal("a Box of a type with Clone is a new allocation holding the method's result")
	}
	if cloned.manual.text != "m+clone" || cloned.manual.cache != nil {
		t.Fatalf("a Clone method on the pointer was not used: %+v", cloned.manual)
	}
	if cloned.ptr == original.ptr || cloned.ptr.text != "p+clone" {
		t.Fatalf("a Box of a type with a pointer Clone: %+v", cloned.ptr)
	}

	senders := 1
	sender := &probeSender{channel: &[]int{}, senders: &senders}
	holder := struct{ tx *probeSender }{tx: sender}
	heldClone := Clone(holder)
	if heldClone.tx == sender || heldClone.tx.channel != sender.channel || senders != 2 {
		t.Fatal("a handle is cloned by its own Clone, which returns the pointer")
	}
	if direct := Clone(sender); direct == sender || direct.channel != sender.channel || senders != 3 {
		t.Fatal("Clone of a handle pointer must use the handle's Clone")
	}

	object := probeObject{{Key: "k", Value: 1}}
	objectClone := Clone(object)
	if cap(objectClone) != 5 {
		t.Fatal("a named slice with a Clone method is cloned by it")
	}
	nested := Clone(struct{ Meta probeObject }{Meta: object})
	if cap(nested.Meta) != 5 {
		t.Fatal("a named slice with a Clone method is cloned by it inside a struct")
	}
	var boxedObject any = object
	if cap(Clone(boxedObject).(probeObject)) != 5 {
		t.Fatal("a named slice with a Clone method is cloned by it inside an interface")
	}

	var expr probeExpr = &probeExpr_Lit{value: 4}
	if got := Clone(expr).(*probeExpr_Lit); got.value != 8 {
		t.Fatalf("an enum's hand-written Clone was not used: %+v", got)
	}
	inStruct := Clone(struct{ e probeExpr }{e: expr})
	if inStruct.e.(*probeExpr_Lit).value != 8 || inStruct.e == expr {
		t.Fatalf("an enum's hand-written Clone was not used inside a struct: %+v", inStruct.e)
	}
}

func TestCloneRebuildsCollections(t *testing.T) {
	var tree probeTreeMap[string, []int]
	tree.Insert("b", []int{2})
	tree.Insert("a", []int{1})
	treeClone := Clone(tree)
	if !slices.Equal(treeClone.keys, []string{"a", "b"}) {
		t.Fatalf("keys = %v", treeClone.keys)
	}
	(*treeClone.Get("a"))[0] = 9
	treeClone.Insert("c", nil)
	if (*tree.Get("a"))[0] != 1 || tree.ReflectLen() != 2 {
		t.Fatal("the collection or its values are shared")
	}

	var index probeHashMap[probeColor, probeLeaf]
	index.Insert(probeColor_Blue, newProbeLeaf("blue"))
	index.Insert(probeColor_Red, newProbeLeaf("red"))
	indexClone := Clone(index)
	if !slices.Equal(indexClone.keys, []probeColor{probeColor_Blue, probeColor_Red}) {
		t.Fatalf("an insertion-ordered map lost its order: %v", indexClone.keys)
	}
	if indexClone.Get(probeColor_Red).box == index.Get(probeColor_Red).box {
		t.Fatal("the values of a collection must be cloned")
	}

	var queue probeDeque[probeShape]
	queue.PushBack(&probeShape_Circle{F0: 1})
	queue.PushBack(&probeShape_Rect{Width: 2})
	queueClone := Clone(queue)
	if queueClone.ReflectLen() != 2 || *queueClone.items[0] == *queue.items[0] {
		t.Fatal("a deque of enum values must hold copies")
	}
	if !reflect.DeepEqual(*queueClone.items[1], *queue.items[1]) {
		t.Fatal("a deque lost its order")
	}

	var set probeTreeSet[int]
	for _, n := range []int{3, 1, 2} {
		set.Insert(n)
	}
	setClone := Clone(set)
	setClone.Insert(4)
	if set.ReflectLen() != 3 || setClone.ReflectLen() != 4 {
		t.Fatal("a set is shared")
	}

	holder := struct {
		byName probeTreeMap[string, *probeBox]
		empty  probeHashMap[string, int]
	}{}
	holder.byName.Insert("x", &probeBox{values: []int{1}})
	holderClone := Clone(holder)
	if *holderClone.byName.Get("x") == *holder.byName.Get("x") {
		t.Fatal("a Box in a collection must be a new allocation")
	}
	if holderClone.empty.ReflectLen() != 0 {
		t.Fatal("an empty collection clones to an empty one")
	}

	// A map keyed by enum values.
	var keyed probeHashMap[probeShape, string]
	key := &probeShape_Rect{Width: 1}
	keyed.Insert(key, "rect")
	keyedClone := Clone(keyed)
	if keyedClone.keys[0] == probeShape(key) || keyedClone.keys[0].(*probeShape_Rect).Width != 1 {
		t.Fatal("an enum key must be copied")
	}
}

// probeMethodMap is a collection with Clone, Equal and Compare methods of
// its own, as those of go/rs/collections have for direct calls. The
// structural operations must go through ReflectMap instead.
type probeMethodMap[K cmp.Ordered, V any] struct {
	probeTreeMap[K, V]
}

func (m probeMethodMap[K, V]) Clone() probeMethodMap[K, V] {
	panic("the structural clone called the collection's own Clone")
}

func (m probeMethodMap[K, V]) Equal(other *probeMethodMap[K, V]) bool {
	panic("the structural equality called the collection's own Equal")
}

func (m probeMethodMap[K, V]) Compare(other *probeMethodMap[K, V]) int {
	panic("the structural order called the collection's own Compare")
}

// Hard-won: e641657c7: a collection's own Clone and Compare hid its elements from an arc tag and from partial_cmp.
func TestCloneEqAndCmpReachThroughACollectionsOwnMethods(t *testing.T) {
	shared := &probeBox{values: []int{1}}
	type holder struct {
		Shared probeMethodMap[string, *probeBox] `rs:"shared,arc"`
		Owned  probeMethodMap[string, *probeBox] `rs:"owned"`
	}
	var original holder
	original.Shared.Insert("k", shared)
	original.Owned.Insert("k", shared)
	cloned := Clone(original)
	if *cloned.Shared.Get("k") != shared {
		t.Fatal("an arc tag must reach the Arcs in a collection")
	}
	if *cloned.Owned.Get("k") == shared || (*cloned.Owned.Get("k")).values[0] != 1 {
		t.Fatal("a Box in a collection is a new allocation")
	}
	if top := Clone(original.Owned); *top.Get("k") == shared {
		t.Fatal("Clone of the collection itself copies its boxes")
	}
	if !Eq(original, cloned) || Cmp(original, cloned) != 0 || !Eq(original.Owned, cloned.Owned) {
		t.Fatal("a collection is equal to its clone")
	}

	var nan, one probeMethodMap[string, float64]
	nan.Insert("a", math.NaN())
	one.Insert("a", 1)
	if PartialCmp(nan, one).IsSome() || PartialCmp(probeFloatHolder{nan}, probeFloatHolder{one}).IsSome() || Eq(nan, nan) {
		t.Fatal("a NaN in a collection leaves it unordered and unequal to itself")
	}
}

type probeFloatHolder struct {
	values probeMethodMap[string, float64]
}

// probeHandle is a runtime handle: it embeds the marker.
type probeHandle struct {
	NoReflectClone
	state *[]int
	id    int
}

// probeGuarded holds a lock by value.
type probeGuarded struct {
	mu    sync.Mutex
	items []int
}

func TestCloneNeverCopiesLocksAndHandlesFieldByField(t *testing.T) {
	state := []int{1}
	handle := &probeHandle{state: &state, id: 7}
	guarded := &probeGuarded{items: []int{1}}
	counter := &atomic.Int64{}
	counter.Store(5)
	wrapped := &struct {
		inner probeHandle
		data  []int
	}{inner: *handle, data: []int{1}}

	original := struct {
		byValue probeHandle
		handle  *probeHandle
		guarded *probeGuarded
		lock    *sync.Mutex
		counter *atomic.Int64
		wrapped *struct {
			inner probeHandle
			data  []int
		}
		anything any
	}{
		byValue:  *handle,
		handle:   handle,
		guarded:  guarded,
		lock:     &sync.Mutex{},
		counter:  counter,
		wrapped:  wrapped,
		anything: handle,
	}
	original.guarded.mu.Lock()
	cloned := Clone(original)
	original.guarded.mu.Unlock()

	if cloned.handle != original.handle {
		t.Error("a pointer to a NoReflectClone type must be shared")
	}
	if cloned.byValue.state != original.byValue.state || cloned.byValue.id != 7 {
		t.Error("a NoReflectClone value must be copied by assignment")
	}
	if cloned.guarded != original.guarded {
		t.Error("a pointer to a struct holding a lock must be shared")
	}
	if cloned.lock != original.lock || cloned.counter != original.counter {
		t.Error("pointers to sync and sync/atomic types must be shared")
	}
	if cloned.wrapped != original.wrapped {
		t.Error("a pointer to a struct holding a NoReflectClone value must be shared")
	}
	if cloned.anything != original.anything {
		t.Error("a handle in an interface must be shared")
	}
}

func TestCloneTreatsStandardLibraryTypesAsValues(t *testing.T) {
	moment := time.Date(2026, 11, 20, 12, 0, 0, 0, time.FixedZone("x", 3600))
	buffer := bytes.NewBufferString("buffered")
	pattern := regexp.MustCompile("a+")
	original := struct {
		at      time.Time
		atBox   *time.Time
		buffer  *bytes.Buffer
		pattern *regexp.Regexp
		span    time.Duration
	}{at: moment, atBox: &moment, buffer: buffer, pattern: pattern, span: time.Second}
	cloned := Clone(original)
	if !cloned.at.Equal(moment) || cloned.at.Location() != moment.Location() || cloned.span != time.Second {
		t.Error("a time.Time is copied by assignment")
	}
	if cloned.atBox == original.atBox || !cloned.atBox.Equal(moment) {
		t.Error("a pointer to a comparable standard library value is a Box of it")
	}
	if cloned.buffer != buffer || cloned.pattern != pattern {
		t.Error("a pointer to a standard library type that cannot be compared is a handle and is shared")
	}
}

// probeNoisy panics when its Clone runs.
type probeNoisy struct{ x int }

func (probeNoisy) Clone() probeNoisy { panic("Cell must not clone its value") }

func TestCloneOfCellsAndRefCells(t *testing.T) {
	// Port of coretests cell.rs cell_does_not_clone: using a Cell in any way
	// never invokes the value's clone().
	cell := CellNew(probeNoisy{x: 22})
	_ = cell.Get()
	if got := Clone(cell); got.Get().x != 22 {
		t.Fatalf("Clone of a Cell = %+v", got)
	}

	refCell := RefCellNew([]int{1, 2})
	refClone := Clone(refCell)
	(*refClone.BorrowMut())[0] = 9
	if (*refCell.Borrow())[0] != 1 {
		t.Fatal("RefCell::clone clones the value")
	}

	holder := struct {
		scroll Cell[int]
		areas  RefCell[[]string]
	}{scroll: CellNew(3), areas: RefCellNew([]string{"a"})}
	holderClone := Clone(holder)
	holderClone.scroll.Set(4)
	(*holderClone.areas.BorrowMut())[0] = "b"
	if holder.scroll.Get() != 3 || (*holder.areas.Borrow())[0] != "a" {
		t.Fatal("cells inside a struct are shared")
	}
}

type probeNode struct {
	value    int
	children []*probeNode
	parent   Option[*probeNode] `rs:"parent,arc"`
	extra    map[string]probeNode
}

type probeGeneric[K comparable, V any] struct {
	key    K
	values []V
	next   *probeGeneric[K, V]
}

func TestCloneRecursiveAndGenericTypes(t *testing.T) {
	root := &probeNode{value: 1}
	child := &probeNode{value: 2, parent: Some(root), extra: map[string]probeNode{"e": {value: 3}}}
	root.children = []*probeNode{child, {value: 4}}
	cloned := Clone(root)
	if cloned == root || cloned.children[0] == child || cloned.children[0].value != 2 {
		t.Fatal("a tree must be copied node by node")
	}
	if cloned.children[0].parent.Unwrap() != root {
		t.Fatal("an Arc back to the parent is shared")
	}
	cloned.children[1].value = 40
	if root.children[1].value != 4 {
		t.Fatal("tree nodes are shared")
	}

	generic := probeGeneric[string, []int]{key: "k", values: [][]int{{1}}, next: &probeGeneric[string, []int]{key: "n"}}
	genericClone := Clone(generic)
	genericClone.values[0][0] = 9
	if generic.values[0][0] != 1 || genericClone.next == generic.next || genericClone.next.key != "n" {
		t.Fatal("a generic struct is cloned like any other")
	}
}

func TestCloneSkipsBlankFieldsAndHandlesEmptyTypes(t *testing.T) {
	type padded struct {
		_     [0]func()
		Value []int
		_     int32
		Unit  Unit
		Empty struct{}
		Zero  [0]int
	}
	original := padded{Value: []int{1}}
	cloned := Clone(original)
	cloned.Value[0] = 2
	if original.Value[0] != 1 {
		t.Fatal("fields after a blank field are shared")
	}
	if got := Clone([]Unit{{}, {}}); len(got) != 2 {
		t.Fatalf("a Vec<()> of two cloned to %d", len(got))
	}
}

func TestCloneFromManyGoroutines(t *testing.T) {
	// Shapes are built on first use; every goroutine must see finished ones.
	type fresh struct {
		leaf  probeLeaf
		shape probeShape
		tree  probeTreeMap[string, []probeLeaf]
		node  *probeNode
	}
	var wait sync.WaitGroup
	for worker := range 16 {
		wait.Add(1)
		go func() {
			defer wait.Done()
			for round := range 50 {
				value := fresh{
					leaf:  newProbeLeaf("w"),
					shape: &probeShape_Group{F0: []probeShape{&probeShape_Rect{Width: uint32(worker), Tags: []string{"t"}}}},
					node:  &probeNode{value: round, children: []*probeNode{{value: worker}}},
				}
				value.tree.Insert("k", []probeLeaf{newProbeLeaf("tree")})
				cloned := Clone(value)
				if !reflect.DeepEqual(value, cloned) || cloned.node == value.node || !Eq(value, cloned) || Cmp(value.leaf.Label, cloned.leaf.Label) != 0 {
					t.Errorf("worker %d round %d: clone differs", worker, round)
					return
				}
			}
		}()
	}
	wait.Wait()
}
