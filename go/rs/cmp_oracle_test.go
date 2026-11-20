package rs_test

import (
	"bytes"
	"encoding/json"
	"math"
	"os"
	"reflect"
	"slices"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// Differential tests of the structural operations (clone.go, eq.go, cmp.go,
// default.go, arc.go) against what #[derive(Clone, PartialEq, PartialOrd,
// Ord, Default)], Ordering, Arc and Weak do in Rust, through the oracle
// module std_cmp (go/port/oracle/src/std_cmp.rs). The Go types below mirror
// the Rust ones declared there, variant for variant and field for field.
//
// The corpora under testdata/oracle are what Rust answered when they were
// recorded:
//
//	MJ_ORACLE=record go/port/gotest.sh ./go/rs/ -run TestStdCmpOracle
//
// MJ_ORACLE=live asks Rust again. With RS_CMP_ORACLE_SCALE=n as well, n
// times as many random cases are generated, for a longer comparison than the
// recorded corpora hold; do not record with it set.

// cmpoRng is a splitmix64 generator: the same sequence everywhere, whatever
// the Go release.
//
// It can also replay the draws another generator made, giving a fresh draw
// at one position instead: a value generated from the replay is then the
// first value with one decision changed, so that pairs of values are close
// enough for their comparison to look past the first field.
type cmpoRng struct {
	state uint64
	// record, when recording, collects every draw.
	record    []uint64
	recording bool
	// replay is given back draw by draw, except at the position flip, and
	// once it runs out.
	replay []uint64
	flip   int
	pos    int
}

func (r *cmpoRng) fresh() uint64 {
	r.state += 0x9E3779B97F4A7C15
	z := r.state
	z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
	z = (z ^ (z >> 27)) * 0x94D049BB133111EB
	return z ^ (z >> 31)
}

func (r *cmpoRng) next() uint64 {
	var draw uint64
	if r.pos < len(r.replay) && r.pos != r.flip {
		draw = r.replay[r.pos]
	} else {
		draw = r.fresh()
	}
	r.pos++
	if r.recording {
		r.record = append(r.record, draw)
	}
	return draw
}

func (r *cmpoRng) intn(n int) int { return int(r.next() % uint64(n)) }

func cmpoPick[T any](r *cmpoRng, items []T) T { return items[r.intn(len(items))] }

// pair generates two values with gen: the same value, the same value with
// one decision changed, or two unrelated values.
func (r *cmpoRng) pair(gen func(g *cmpoRng) any) (a, b any) {
	first := &cmpoRng{state: r.fresh(), recording: true}
	a = gen(first)
	switch r.intn(10) {
	case 0, 1, 2:
		b = gen(&cmpoRng{replay: first.record, flip: -1})
	case 3, 4, 5, 6, 7, 8:
		b = gen(&cmpoRng{state: r.fresh(), replay: first.record, flip: r.intn(len(first.record))})
	default:
		b = gen(&cmpoRng{state: r.fresh()})
	}
	return a, b
}

// cmpoScale is how many times the usual number of random cases to generate.
func cmpoScale() int {
	if os.Getenv("MJ_ORACLE") == "live" {
		if scale, err := strconv.Atoi(os.Getenv("RS_CMP_ORACLE_SCALE")); err == nil && scale > 0 {
			return scale
		}
	}
	return 1
}

// Leaves.

func (r *cmpoRng) signed(bits uint) int64 {
	low, high := int64(-1)<<(bits-1), int64(uint64(1)<<(bits-1)-1)
	switch r.intn(4) {
	case 0:
		return cmpoPick(r, []int64{0, 1, -1, 2, low, high, low + 1, high - 1})
	case 1, 2:
		return int64(r.intn(7)) - 3
	}
	return int64(r.next()<<(64-bits)) >> (64 - bits)
}

func (r *cmpoRng) unsigned(bits uint) uint64 {
	high := ^uint64(0) >> (64 - bits)
	switch r.intn(4) {
	case 0:
		return cmpoPick(r, []uint64{0, 1, 2, high, high - 1, high>>1 + 1, high >> 1})
	case 1, 2:
		return uint64(r.intn(5))
	}
	return r.next() >> (64 - bits)
}

var cmpoFloatEdges = []float64{
	0, math.Copysign(0, -1), 1, -1, 0.5, -0.5, 1.5, 2, 3, 0.1, 1e16, 1e-300, -1e300,
	5e-324, -5e-324, math.MaxFloat64, -math.MaxFloat64, math.Inf(1), math.Inf(-1), math.NaN(),
	math.MaxFloat32, math.SmallestNonzeroFloat32,
}

// float returns the text of a float of the given width, as both sides read
// it: strconv.ParseFloat and str::parse agree on every float's text.
func (r *cmpoRng) float(bits int) string {
	var value float64
	switch r.intn(10) {
	case 0:
		// Often enough that an order is decided by a NaN at any depth.
		value = math.NaN()
	case 1, 2, 3, 4:
		value = cmpoPick(r, cmpoFloatEdges)
	case 5, 6:
		value = float64(r.intn(7)) - 3
	default:
		if bits == 32 {
			value = float64(math.Float32frombits(uint32(r.next())))
		} else {
			value = math.Float64frombits(r.next())
		}
	}
	if bits == 32 {
		return strconv.FormatFloat(float64(float32(value)), 'g', -1, 32)
	}
	return strconv.FormatFloat(value, 'g', -1, 64)
}

// cmpoChars are the chars strings are made of: few, so that strings share
// prefixes, and of every UTF-8 length, so that byte order is tested.
var cmpoChars = []rune{'a', 'a', 'b', 'b', 'z', 'A', 'Z', '0', ' ', '~', 0x7f, 0, 'é', 'ß', 0xff, 0x100, '日', 0xfffd, 0xffff, 0x10000, '😀', 0x10ffff}

func (r *cmpoRng) char() string { return string(cmpoPick(r, cmpoChars)) }

func (r *cmpoRng) text(maxLen int) string {
	var out []rune
	for range r.intn(maxLen + 1) {
		out = append(out, cmpoPick(r, cmpoChars))
	}
	return string(out)
}

func cmpoRepeat(r *cmpoRng, maxLen int, item func() any) []any {
	out := []any{}
	for range r.intn(maxLen + 1) {
		out = append(out, item())
	}
	return out
}

// The values of Val, the enum of std_cmp.rs that holds one of every shape.

type cmpoVal interface{ isCmpoVal() }

type (
	cmpoVal_Unit       struct{}
	cmpoVal_Bool       struct{ F0 bool }
	cmpoVal_I8         struct{ F0 int8 }
	cmpoVal_I16        struct{ F0 int16 }
	cmpoVal_I32        struct{ F0 int32 }
	cmpoVal_I64        struct{ F0 int64 }
	cmpoVal_Isize      struct{ F0 int }
	cmpoVal_U8         struct{ F0 uint8 }
	cmpoVal_U16        struct{ F0 uint16 }
	cmpoVal_U32        struct{ F0 uint32 }
	cmpoVal_U64        struct{ F0 uint64 }
	cmpoVal_Usize      struct{ F0 int }
	cmpoVal_F32        struct{ F0 float32 }
	cmpoVal_F64        struct{ F0 float64 }
	cmpoVal_Char       struct{ F0 rune }
	cmpoVal_Str        struct{ F0 string }
	cmpoVal_Bytes      struct{ F0 []byte }
	cmpoVal_Ints       struct{ F0 []int64 }
	cmpoVal_Floats     struct{ F0 []float64 }
	cmpoVal_Strs       struct{ F0 []string }
	cmpoVal_Array      struct{ F0 [3]int16 }
	cmpoVal_Grid       struct{ F0 [2][2]bool }
	cmpoVal_MaybeInt   struct{ F0 rs.Option[int32] }
	cmpoVal_MaybeFloat struct{ F0 rs.Option[float64] }
	cmpoVal_MaybeVal   struct{ F0 rs.Option[cmpoVal] }
	cmpoVal_List       struct{ F0 []cmpoVal }
	cmpoVal_Pair       struct{ F0 rs.Tuple2[int32, string] }
	cmpoVal_Triple     struct {
		F0 cmpoVal
		F1 float64
		F2 bool
	}
	cmpoVal_Rec struct {
		Id    uint32             `rs:"id"`
		Name  string             `rs:"name"`
		Score float64            `rs:"score"`
		Tags  []string           `rs:"tags"`
		Child rs.Option[cmpoVal] `rs:"child"`
	}
	cmpoVal_Point   struct{ F0 cmpoPoint }
	cmpoVal_Color   struct{ F0 cmpoColor }
	cmpoVal_Res     struct{ F0 rs.Result[int32] }
	cmpoVal_Rev     struct{ F0 rs.Reverse[int32] }
	cmpoVal_RevF    struct{ F0 rs.Reverse[float64] }
	cmpoVal_Map     struct{ F0 cmpoTreeMap[cmpoKey, cmpoVal] }
	cmpoVal_Set     struct{ F0 cmpoTreeSet[cmpoKey] }
	cmpoVal_Deque   struct{ F0 cmpoDeque[int8] }
	cmpoVal_Boxed   struct{ F0 cmpoVal }
	cmpoVal_Key     struct{ F0 cmpoKey }
	cmpoVal_Cell    struct{ F0 rs.Cell[int32] }
	cmpoVal_RefCell struct{ F0 rs.RefCell[[]int32] }
	// Counted is Arc<f64> as a counted rs.Arc, Shared Arc<Point> as a plain
	// pointer.
	cmpoVal_Counted struct{ F0 rs.Arc[float64] }
	cmpoVal_Shared  struct {
		F0 *cmpoPoint `rs:",arc"`
	}
	cmpoVal_Void struct{ F0 rs.Unit }
)

func (*cmpoVal_Unit) isCmpoVal()       {}
func (*cmpoVal_Bool) isCmpoVal()       {}
func (*cmpoVal_I8) isCmpoVal()         {}
func (*cmpoVal_I16) isCmpoVal()        {}
func (*cmpoVal_I32) isCmpoVal()        {}
func (*cmpoVal_I64) isCmpoVal()        {}
func (*cmpoVal_Isize) isCmpoVal()      {}
func (*cmpoVal_U8) isCmpoVal()         {}
func (*cmpoVal_U16) isCmpoVal()        {}
func (*cmpoVal_U32) isCmpoVal()        {}
func (*cmpoVal_U64) isCmpoVal()        {}
func (*cmpoVal_Usize) isCmpoVal()      {}
func (*cmpoVal_F32) isCmpoVal()        {}
func (*cmpoVal_F64) isCmpoVal()        {}
func (*cmpoVal_Char) isCmpoVal()       {}
func (*cmpoVal_Str) isCmpoVal()        {}
func (*cmpoVal_Bytes) isCmpoVal()      {}
func (*cmpoVal_Ints) isCmpoVal()       {}
func (*cmpoVal_Floats) isCmpoVal()     {}
func (*cmpoVal_Strs) isCmpoVal()       {}
func (*cmpoVal_Array) isCmpoVal()      {}
func (*cmpoVal_Grid) isCmpoVal()       {}
func (*cmpoVal_MaybeInt) isCmpoVal()   {}
func (*cmpoVal_MaybeFloat) isCmpoVal() {}
func (*cmpoVal_MaybeVal) isCmpoVal()   {}
func (*cmpoVal_List) isCmpoVal()       {}
func (*cmpoVal_Pair) isCmpoVal()       {}
func (*cmpoVal_Triple) isCmpoVal()     {}
func (*cmpoVal_Rec) isCmpoVal()        {}
func (*cmpoVal_Point) isCmpoVal()      {}
func (*cmpoVal_Color) isCmpoVal()      {}
func (*cmpoVal_Res) isCmpoVal()        {}
func (*cmpoVal_Rev) isCmpoVal()        {}
func (*cmpoVal_RevF) isCmpoVal()       {}
func (*cmpoVal_Map) isCmpoVal()        {}
func (*cmpoVal_Set) isCmpoVal()        {}
func (*cmpoVal_Deque) isCmpoVal()      {}
func (*cmpoVal_Boxed) isCmpoVal()      {}
func (*cmpoVal_Key) isCmpoVal()        {}
func (*cmpoVal_Cell) isCmpoVal()       {}
func (*cmpoVal_RefCell) isCmpoVal()    {}
func (*cmpoVal_Counted) isCmpoVal()    {}
func (*cmpoVal_Shared) isCmpoVal()     {}
func (*cmpoVal_Void) isCmpoVal()       {}

var _ = rs.RegisterEnum[cmpoVal]("Val",
	rs.Variant[cmpoVal_Unit]("Unit", rs.ShapeUnit),
	rs.Variant[cmpoVal_Bool]("Bool", rs.ShapeTuple),
	rs.Variant[cmpoVal_I8]("I8", rs.ShapeTuple),
	rs.Variant[cmpoVal_I16]("I16", rs.ShapeTuple),
	rs.Variant[cmpoVal_I32]("I32", rs.ShapeTuple),
	rs.Variant[cmpoVal_I64]("I64", rs.ShapeTuple),
	rs.Variant[cmpoVal_Isize]("Isize", rs.ShapeTuple),
	rs.Variant[cmpoVal_U8]("U8", rs.ShapeTuple),
	rs.Variant[cmpoVal_U16]("U16", rs.ShapeTuple),
	rs.Variant[cmpoVal_U32]("U32", rs.ShapeTuple),
	rs.Variant[cmpoVal_U64]("U64", rs.ShapeTuple),
	rs.Variant[cmpoVal_Usize]("Usize", rs.ShapeTuple),
	rs.Variant[cmpoVal_F32]("F32", rs.ShapeTuple),
	rs.Variant[cmpoVal_F64]("F64", rs.ShapeTuple),
	rs.Variant[cmpoVal_Char]("Char", rs.ShapeTuple),
	rs.Variant[cmpoVal_Str]("Str", rs.ShapeTuple),
	rs.Variant[cmpoVal_Bytes]("Bytes", rs.ShapeTuple),
	rs.Variant[cmpoVal_Ints]("Ints", rs.ShapeTuple),
	rs.Variant[cmpoVal_Floats]("Floats", rs.ShapeTuple),
	rs.Variant[cmpoVal_Strs]("Strs", rs.ShapeTuple),
	rs.Variant[cmpoVal_Array]("Array", rs.ShapeTuple),
	rs.Variant[cmpoVal_Grid]("Grid", rs.ShapeTuple),
	rs.Variant[cmpoVal_MaybeInt]("MaybeInt", rs.ShapeTuple),
	rs.Variant[cmpoVal_MaybeFloat]("MaybeFloat", rs.ShapeTuple),
	rs.Variant[cmpoVal_MaybeVal]("MaybeVal", rs.ShapeTuple),
	rs.Variant[cmpoVal_List]("List", rs.ShapeTuple),
	rs.Variant[cmpoVal_Pair]("Pair", rs.ShapeTuple),
	rs.Variant[cmpoVal_Triple]("Triple", rs.ShapeTuple),
	rs.Variant[cmpoVal_Rec]("Rec", rs.ShapeStruct),
	rs.Variant[cmpoVal_Point]("Point", rs.ShapeTuple),
	rs.Variant[cmpoVal_Color]("Color", rs.ShapeTuple),
	rs.Variant[cmpoVal_Res]("Res", rs.ShapeTuple),
	rs.Variant[cmpoVal_Rev]("Rev", rs.ShapeTuple),
	rs.Variant[cmpoVal_RevF]("RevF", rs.ShapeTuple),
	rs.Variant[cmpoVal_Map]("Map", rs.ShapeTuple),
	rs.Variant[cmpoVal_Set]("Set", rs.ShapeTuple),
	rs.Variant[cmpoVal_Deque]("Deque", rs.ShapeTuple),
	rs.Variant[cmpoVal_Boxed]("Boxed", rs.ShapeTuple),
	rs.Variant[cmpoVal_Key]("Key", rs.ShapeTuple),
	rs.Variant[cmpoVal_Cell]("Cell", rs.ShapeTuple),
	rs.Variant[cmpoVal_RefCell]("RefCell", rs.ShapeTuple),
	rs.Variant[cmpoVal_Counted]("Counted", rs.ShapeTuple),
	rs.Variant[cmpoVal_Shared]("Shared", rs.ShapeTuple),
	rs.Variant[cmpoVal_Void]("Void", rs.ShapeTuple),
)

type cmpoPoint struct {
	X     int32  `rs:"x"`
	Y     int32  `rs:"y"`
	Label string `rs:"label"`
}

var _ = rs.RegisterStruct[cmpoPoint]("Point")

// cmpoColor is ordered by its discriminants, which are not in declaration
// order.
type cmpoColor uint8

const (
	cmpoColor_Red   cmpoColor = 2
	cmpoColor_Green cmpoColor = 0
	cmpoColor_Blue  cmpoColor = 1
)

var _ = rs.RegisterEnum[cmpoColor]("Color",
	rs.Const(cmpoColor_Red, "Red"),
	rs.Const(cmpoColor_Green, "Green"),
	rs.Const(cmpoColor_Blue, "Blue"),
)

// cmpoErr is the String error of Result<i32, String>.
type cmpoErr struct{ text string }

func (e *cmpoErr) Error() string { return e.text }

// Key is the totally ordered enum of std_cmp.rs that maps and sets are keyed
// by.

type cmpoKey interface{ isCmpoKey() }

type (
	cmpoKey_Empty struct{}
	cmpoKey_Num   struct{ F0 int64 }
	cmpoKey_Text  struct{ F0 string }
	cmpoKey_Pair  struct {
		F0 uint8
		F1 bool
	}
	cmpoKey_Nested struct{ F0 []cmpoKey }
	cmpoKey_Tagged struct {
		Tag   rune               `rs:"tag"`
		Inner rs.Option[cmpoKey] `rs:"inner"`
	}
)

func (*cmpoKey_Empty) isCmpoKey()  {}
func (*cmpoKey_Num) isCmpoKey()    {}
func (*cmpoKey_Text) isCmpoKey()   {}
func (*cmpoKey_Pair) isCmpoKey()   {}
func (*cmpoKey_Nested) isCmpoKey() {}
func (*cmpoKey_Tagged) isCmpoKey() {}

var _ = rs.RegisterEnum[cmpoKey]("Key",
	rs.Variant[cmpoKey_Empty]("Empty", rs.ShapeUnit),
	rs.Variant[cmpoKey_Num]("Num", rs.ShapeTuple),
	rs.Variant[cmpoKey_Text]("Text", rs.ShapeTuple),
	rs.Variant[cmpoKey_Pair]("Pair", rs.ShapeTuple),
	rs.Variant[cmpoKey_Nested]("Nested", rs.ShapeTuple),
	rs.Variant[cmpoKey_Tagged]("Tagged", rs.ShapeStruct),
)

// Collections, as the collections of go/rs/collections describe themselves
// to the structural operations.

// cmpoTreeMap keeps its entries in key order, as a BTreeMap does.
type cmpoTreeMap[K, V any] struct {
	keys   []K
	values []*V
}

// insert is BTreeMap::insert: an equal key keeps its place and takes the new
// value.
func (m *cmpoTreeMap[K, V]) insert(key K, value V) {
	at, found := slices.BinarySearchFunc(m.keys, key, rs.Cmp[K])
	if found {
		m.values[at] = &value
		return
	}
	m.keys = slices.Insert(m.keys, at, key)
	m.values = slices.Insert(m.values, at, &value)
}

func (m *cmpoTreeMap[K, V]) ReflectLen() int                  { return len(m.keys) }
func (m *cmpoTreeMap[K, V]) ReflectKeyType() reflect.Type     { return reflect.TypeFor[K]() }
func (m *cmpoTreeMap[K, V]) ReflectValueType() reflect.Type   { return reflect.TypeFor[V]() }
func (m *cmpoTreeMap[K, V]) ReflectInsert(key any, value any) { m.insert(key.(K), value.(V)) }
func (m *cmpoTreeMap[K, V]) ReflectSorted() bool              { return true }
func (m *cmpoTreeMap[K, V]) ReflectEntries(yield func(key any, value any) bool) {
	for at, key := range m.keys {
		if !yield(key, m.values[at]) {
			return
		}
	}
}

// cmpoTreeSet keeps its elements in order, as a BTreeSet does.
type cmpoTreeSet[T any] struct{ items []*T }

func (s *cmpoTreeSet[T]) insert(item T) {
	at, found := slices.BinarySearchFunc(s.items, item, func(have *T, want T) int { return rs.Cmp(*have, want) })
	if !found {
		s.items = slices.Insert(s.items, at, &item)
	}
}

func (s *cmpoTreeSet[T]) ReflectLen() int               { return len(s.items) }
func (s *cmpoTreeSet[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }
func (s *cmpoTreeSet[T]) ReflectPush(element any)       { s.insert(element.(T)) }
func (s *cmpoTreeSet[T]) ReflectIsSet() bool            { return true }
func (s *cmpoTreeSet[T]) ReflectSorted() bool           { return true }
func (s *cmpoTreeSet[T]) ReflectElements(yield func(element any) bool) {
	for _, item := range s.items {
		if !yield(item) {
			return
		}
	}
}

// cmpoDeque is a sequence, as a VecDeque is.
type cmpoDeque[T any] struct{ items []*T }

func (d *cmpoDeque[T]) ReflectLen() int               { return len(d.items) }
func (d *cmpoDeque[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }
func (d *cmpoDeque[T]) ReflectPush(element any) {
	item := element.(T)
	d.items = append(d.items, &item)
}
func (d *cmpoDeque[T]) ReflectIsSet() bool  { return false }
func (d *cmpoDeque[T]) ReflectSorted() bool { return false }
func (d *cmpoDeque[T]) ReflectElements(yield func(element any) bool) {
	for _, item := range d.items {
		if !yield(item) {
			return
		}
	}
}

// cmpoHashMap keeps its entries in insertion order and says that the order
// means nothing, as a HashMap does.
type cmpoHashMap[K, V any] struct {
	keys   []K
	values []*V
}

// insert is HashMap::insert: an equal key keeps its place and takes the new
// value.
func (m *cmpoHashMap[K, V]) insert(key K, value V) {
	for at, have := range m.keys {
		if rs.Eq(have, key) {
			m.values[at] = &value
			return
		}
	}
	m.keys = append(m.keys, key)
	m.values = append(m.values, &value)
}

func (m *cmpoHashMap[K, V]) ReflectLen() int                  { return len(m.keys) }
func (m *cmpoHashMap[K, V]) ReflectKeyType() reflect.Type     { return reflect.TypeFor[K]() }
func (m *cmpoHashMap[K, V]) ReflectValueType() reflect.Type   { return reflect.TypeFor[V]() }
func (m *cmpoHashMap[K, V]) ReflectInsert(key any, value any) { m.insert(key.(K), value.(V)) }
func (m *cmpoHashMap[K, V]) ReflectSorted() bool              { return false }
func (m *cmpoHashMap[K, V]) ReflectEntries(yield func(key any, value any) bool) {
	for at, key := range m.keys {
		if !yield(key, m.values[at]) {
			return
		}
	}
}

// cmpoHashSet keeps its elements in insertion order and says that the order
// means nothing, as a HashSet does.
type cmpoHashSet[T any] struct{ items []*T }

func (s *cmpoHashSet[T]) insert(item T) {
	for _, have := range s.items {
		if rs.Eq(*have, item) {
			return
		}
	}
	s.items = append(s.items, &item)
}

func (s *cmpoHashSet[T]) ReflectLen() int               { return len(s.items) }
func (s *cmpoHashSet[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }
func (s *cmpoHashSet[T]) ReflectPush(element any)       { s.insert(element.(T)) }
func (s *cmpoHashSet[T]) ReflectIsSet() bool            { return true }
func (s *cmpoHashSet[T]) ReflectSorted() bool           { return false }
func (s *cmpoHashSet[T]) ReflectElements(yield func(element any) bool) {
	for _, item := range s.items {
		if !yield(item) {
			return
		}
	}
}

// Generating values, as the JSON both sides read (std_cmp.rs describes it).

var (
	cmpoLeafTags = []string{
		"Unit", "Bool", "I8", "I16", "I32", "I64", "Isize", "U8", "U16", "U32", "U64", "Usize",
		"F32", "F64", "Char", "Str", "Bytes", "Ints", "Floats", "Strs", "Array", "Grid",
		"MaybeInt", "MaybeFloat", "Pair", "Point", "Color", "Res", "Rev", "RevF", "Set", "Deque",
		"Key", "Cell", "RefCell", "Counted", "Shared", "Void",
	}
	cmpoNestingTags = []string{"MaybeVal", "List", "Triple", "Rec", "Map", "Boxed"}
)

// val generates a VAL nested at most depth levels.
func (r *cmpoRng) val(depth int) []any {
	if depth > 0 && r.intn(3) == 0 {
		return r.valOf(cmpoPick(r, cmpoNestingTags), depth)
	}
	return r.valOf(cmpoPick(r, cmpoLeafTags), depth)
}

func (r *cmpoRng) valOf(tag string, depth int) []any {
	signed := func(bits uint) any { return r.signed(bits) }
	switch tag {
	case "Unit", "Void":
		if tag == "Void" {
			return []any{tag, nil}
		}
		return []any{tag}
	case "Bool":
		return []any{tag, r.intn(2) == 0}
	case "I8":
		return []any{tag, r.signed(8)}
	case "I16":
		return []any{tag, r.signed(16)}
	case "I32", "Rev", "Cell":
		return []any{tag, r.signed(32)}
	case "I64", "Isize":
		return []any{tag, r.signed(64)}
	case "U8":
		return []any{tag, r.unsigned(8)}
	case "U16":
		return []any{tag, r.unsigned(16)}
	case "U32":
		return []any{tag, r.unsigned(32)}
	case "U64":
		return []any{tag, r.unsigned(64)}
	case "Usize":
		// usize is int in Go: values up to isize::MAX.
		return []any{tag, r.unsigned(63)}
	case "F32":
		return []any{tag, r.float(32)}
	case "F64", "RevF", "Counted":
		return []any{tag, r.float(64)}
	case "Char":
		return []any{tag, r.char()}
	case "Str":
		return []any{tag, r.text(4)}
	case "Bytes":
		return []any{tag, cmpoRepeat(r, 4, func() any { return r.unsigned(8) })}
	case "Ints":
		return []any{tag, cmpoRepeat(r, 4, func() any { return r.signed(64) })}
	case "Floats":
		return []any{tag, cmpoRepeat(r, 3, func() any { return r.float(64) })}
	case "Strs":
		return []any{tag, cmpoRepeat(r, 3, func() any { return r.text(3) })}
	case "Array":
		return []any{tag, []any{r.signed(16), r.signed(16), r.signed(16)}}
	case "Grid":
		cell := func() any { return r.intn(2) == 0 }
		return []any{tag, []any{[]any{cell(), cell()}, []any{cell(), cell()}}}
	case "MaybeInt":
		return []any{tag, r.maybe(func() any { return r.signed(32) })}
	case "MaybeFloat":
		return []any{tag, r.maybe(func() any { return r.float(64) })}
	case "MaybeVal":
		return []any{tag, r.maybe(func() any { return r.val(depth - 1) })}
	case "List":
		return []any{tag, cmpoRepeat(r, 3, func() any { return r.val(depth - 1) })}
	case "Pair":
		return []any{tag, r.signed(32), r.text(3)}
	case "Triple":
		return []any{tag, r.val(depth - 1), r.float(64), r.intn(2) == 0}
	case "Rec":
		return []any{tag, map[string]any{
			"id":    r.unsigned(32),
			"name":  r.text(3),
			"score": r.float(64),
			"tags":  cmpoRepeat(r, 2, func() any { return r.text(2) }),
			"child": r.maybe(func() any { return r.val(depth - 1) }),
		}}
	case "Point", "Shared":
		return []any{tag, r.point()}
	case "Color":
		return []any{tag, cmpoPick(r, []string{"Red", "Green", "Blue"})}
	case "Res":
		if r.intn(2) == 0 {
			return []any{tag, map[string]any{"ok": r.signed(32)}}
		}
		return []any{tag, map[string]any{"err": r.text(3)}}
	case "Map":
		return []any{tag, cmpoRepeat(r, 3, func() any { return []any{r.key(1), r.val(depth - 1)} })}
	case "Set":
		return []any{tag, cmpoRepeat(r, 3, func() any { return r.key(1) })}
	case "Deque":
		return []any{tag, cmpoRepeat(r, 4, func() any { return signed(8) })}
	case "Boxed":
		return []any{tag, r.val(depth - 1)}
	case "Key":
		return []any{tag, r.key(2)}
	case "RefCell":
		return []any{tag, cmpoRepeat(r, 3, func() any { return signed(32) })}
	}
	panic("no generator for Val::" + tag)
}

func (r *cmpoRng) maybe(some func() any) any {
	if r.intn(3) == 0 {
		return nil
	}
	return some()
}

func (r *cmpoRng) point() map[string]any {
	return map[string]any{"x": r.signed(32), "y": r.signed(32), "label": r.text(2)}
}

// key generates a KEY nested at most depth levels.
func (r *cmpoRng) key(depth int) []any {
	tags := []string{"Empty", "Num", "Text", "Pair", "Nested", "Tagged"}
	if depth <= 0 {
		tags = tags[:4]
	}
	switch tag := cmpoPick(r, tags); tag {
	case "Empty":
		return []any{tag}
	case "Num":
		return []any{tag, r.signed(64)}
	case "Text":
		return []any{tag, r.text(3)}
	case "Pair":
		return []any{tag, r.unsigned(8), r.intn(2) == 0}
	case "Nested":
		return []any{tag, cmpoRepeat(r, 3, func() any { return r.key(depth - 1) })}
	default:
		return []any{tag, map[string]any{"tag": r.char(), "inner": r.maybe(func() any { return r.key(depth - 1) })}}
	}
}

// Reading values: the JSON of a request, as Go values.

func cmpoParse(raw json.RawMessage) map[string]any {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	var request map[string]any
	if err := decoder.Decode(&request); err != nil {
		panic(err)
	}
	return request
}

func cmpoInt(v any) int64 {
	n, err := v.(json.Number).Int64()
	if err != nil {
		panic(err)
	}
	return n
}

func cmpoUint(v any) uint64 {
	n, err := strconv.ParseUint(string(v.(json.Number)), 10, 64)
	if err != nil {
		panic(err)
	}
	return n
}

func cmpoFloat(v any, bits int) float64 {
	f, err := strconv.ParseFloat(v.(string), bits)
	if err != nil {
		panic(err)
	}
	return f
}

func cmpoRune(v any) rune {
	chars := []rune(v.(string))
	if len(chars) != 1 {
		panic("not one char: " + v.(string))
	}
	return chars[0]
}

// cmpoDecoder reads values.
type cmpoDecoder struct {
	// nilEmpty reads an empty list as a nil slice. One side of a comparison
	// is read with it, so that nil and empty slices meet as two empty Vecs.
	nilEmpty bool
	// arcs are the handles made, to be dropped when the request is done.
	arcs []rs.Arc[float64]
}

func (d *cmpoDecoder) drop() {
	for i := range d.arcs {
		d.arcs[i].Drop()
	}
}

func cmpoList[T any](d *cmpoDecoder, v any, item func(any) T) []T {
	items := v.([]any)
	if len(items) == 0 && d.nilEmpty {
		return nil
	}
	out := make([]T, len(items))
	for i, x := range items {
		out[i] = item(x)
	}
	return out
}

func cmpoOption[T any](v any, some func(any) T) rs.Option[T] {
	if v == nil {
		return rs.None[T]()
	}
	return rs.Some(some(v))
}

func cmpoPointOf(v any) cmpoPoint {
	p := v.(map[string]any)
	return cmpoPoint{X: int32(cmpoInt(p["x"])), Y: int32(cmpoInt(p["y"])), Label: p["label"].(string)}
}

func (d *cmpoDecoder) val(v any) cmpoVal {
	p := v.([]any)
	f64 := func(x any) float64 { return cmpoFloat(x, 64) }
	switch tag := p[0].(string); tag {
	case "Unit":
		return &cmpoVal_Unit{}
	case "Bool":
		return &cmpoVal_Bool{F0: p[1].(bool)}
	case "I8":
		return &cmpoVal_I8{F0: int8(cmpoInt(p[1]))}
	case "I16":
		return &cmpoVal_I16{F0: int16(cmpoInt(p[1]))}
	case "I32":
		return &cmpoVal_I32{F0: int32(cmpoInt(p[1]))}
	case "I64":
		return &cmpoVal_I64{F0: cmpoInt(p[1])}
	case "Isize":
		return &cmpoVal_Isize{F0: int(cmpoInt(p[1]))}
	case "U8":
		return &cmpoVal_U8{F0: uint8(cmpoUint(p[1]))}
	case "U16":
		return &cmpoVal_U16{F0: uint16(cmpoUint(p[1]))}
	case "U32":
		return &cmpoVal_U32{F0: uint32(cmpoUint(p[1]))}
	case "U64":
		return &cmpoVal_U64{F0: cmpoUint(p[1])}
	case "Usize":
		return &cmpoVal_Usize{F0: int(cmpoUint(p[1]))}
	case "F32":
		return &cmpoVal_F32{F0: float32(cmpoFloat(p[1], 32))}
	case "F64":
		return &cmpoVal_F64{F0: f64(p[1])}
	case "Char":
		return &cmpoVal_Char{F0: cmpoRune(p[1])}
	case "Str":
		return &cmpoVal_Str{F0: p[1].(string)}
	case "Bytes":
		return &cmpoVal_Bytes{F0: cmpoList(d, p[1], func(x any) byte { return byte(cmpoUint(x)) })}
	case "Ints":
		return &cmpoVal_Ints{F0: cmpoList(d, p[1], cmpoInt)}
	case "Floats":
		return &cmpoVal_Floats{F0: cmpoList(d, p[1], f64)}
	case "Strs":
		return &cmpoVal_Strs{F0: cmpoList(d, p[1], func(x any) string { return x.(string) })}
	case "Array":
		items := p[1].([]any)
		return &cmpoVal_Array{F0: [3]int16{int16(cmpoInt(items[0])), int16(cmpoInt(items[1])), int16(cmpoInt(items[2]))}}
	case "Grid":
		var grid [2][2]bool
		for i, row := range p[1].([]any) {
			for j, cell := range row.([]any) {
				grid[i][j] = cell.(bool)
			}
		}
		return &cmpoVal_Grid{F0: grid}
	case "MaybeInt":
		return &cmpoVal_MaybeInt{F0: cmpoOption(p[1], func(x any) int32 { return int32(cmpoInt(x)) })}
	case "MaybeFloat":
		return &cmpoVal_MaybeFloat{F0: cmpoOption(p[1], f64)}
	case "MaybeVal":
		return &cmpoVal_MaybeVal{F0: cmpoOption(p[1], d.val)}
	case "List":
		return &cmpoVal_List{F0: cmpoList(d, p[1], d.val)}
	case "Pair":
		return &cmpoVal_Pair{F0: rs.T2(int32(cmpoInt(p[1])), p[2].(string))}
	case "Triple":
		return &cmpoVal_Triple{F0: d.val(p[1]), F1: f64(p[2]), F2: p[3].(bool)}
	case "Rec":
		rec := p[1].(map[string]any)
		return &cmpoVal_Rec{
			Id:    uint32(cmpoUint(rec["id"])),
			Name:  rec["name"].(string),
			Score: f64(rec["score"]),
			Tags:  cmpoList(d, rec["tags"], func(x any) string { return x.(string) }),
			Child: cmpoOption(rec["child"], d.val),
		}
	case "Point":
		return &cmpoVal_Point{F0: cmpoPointOf(p[1])}
	case "Color":
		colors := map[string]cmpoColor{"Red": cmpoColor_Red, "Green": cmpoColor_Green, "Blue": cmpoColor_Blue}
		return &cmpoVal_Color{F0: colors[p[1].(string)]}
	case "Res":
		res := p[1].(map[string]any)
		if ok, found := res["ok"]; found {
			return &cmpoVal_Res{F0: rs.Ok(int32(cmpoInt(ok)))}
		}
		return &cmpoVal_Res{F0: rs.Err[int32](&cmpoErr{text: res["err"].(string)})}
	case "Rev":
		return &cmpoVal_Rev{F0: rs.Reverse[int32]{F0: int32(cmpoInt(p[1]))}}
	case "RevF":
		return &cmpoVal_RevF{F0: rs.Reverse[float64]{F0: f64(p[1])}}
	case "Map":
		value := &cmpoVal_Map{}
		for _, entry := range p[1].([]any) {
			pair := entry.([]any)
			value.F0.insert(d.key(pair[0]), d.val(pair[1]))
		}
		return value
	case "Set":
		value := &cmpoVal_Set{}
		for _, item := range p[1].([]any) {
			value.F0.insert(d.key(item))
		}
		return value
	case "Deque":
		value := &cmpoVal_Deque{}
		for _, item := range p[1].([]any) {
			value.F0.ReflectPush(int8(cmpoInt(item)))
		}
		return value
	case "Boxed":
		return &cmpoVal_Boxed{F0: d.val(p[1])}
	case "Key":
		return &cmpoVal_Key{F0: d.key(p[1])}
	case "Cell":
		return &cmpoVal_Cell{F0: rs.CellNew(int32(cmpoInt(p[1])))}
	case "RefCell":
		return &cmpoVal_RefCell{F0: rs.RefCellNew(cmpoList(d, p[1], func(x any) int32 { return int32(cmpoInt(x)) }))}
	case "Counted":
		arc := rs.ArcNew(f64(p[1]))
		d.arcs = append(d.arcs, arc)
		return &cmpoVal_Counted{F0: arc}
	case "Shared":
		point := cmpoPointOf(p[1])
		return &cmpoVal_Shared{F0: &point}
	case "Void":
		return &cmpoVal_Void{}
	default:
		panic("not a Val tag: " + tag)
	}
}

func (d *cmpoDecoder) key(v any) cmpoKey {
	p := v.([]any)
	switch tag := p[0].(string); tag {
	case "Empty":
		return &cmpoKey_Empty{}
	case "Num":
		return &cmpoKey_Num{F0: cmpoInt(p[1])}
	case "Text":
		return &cmpoKey_Text{F0: p[1].(string)}
	case "Pair":
		return &cmpoKey_Pair{F0: uint8(cmpoUint(p[1])), F1: p[2].(bool)}
	case "Nested":
		return &cmpoKey_Nested{F0: cmpoList(d, p[1], d.key)}
	case "Tagged":
		tagged := p[1].(map[string]any)
		return &cmpoKey_Tagged{Tag: cmpoRune(tagged["tag"]), Inner: cmpoOption(tagged["inner"], d.key)}
	default:
		panic("not a Key tag: " + tag)
	}
}

// cmpoKeyJSON writes a key as std_cmp.rs writes it back.
func cmpoKeyJSON(key cmpoKey) any {
	switch key := key.(type) {
	case *cmpoKey_Empty:
		return []any{"Empty"}
	case *cmpoKey_Num:
		return []any{"Num", key.F0}
	case *cmpoKey_Text:
		return []any{"Text", key.F0}
	case *cmpoKey_Pair:
		return []any{"Pair", key.F0, key.F1}
	case *cmpoKey_Nested:
		items := make([]any, 0, len(key.F0))
		for _, item := range key.F0 {
			items = append(items, cmpoKeyJSON(item))
		}
		return []any{"Nested", items}
	case *cmpoKey_Tagged:
		var inner any
		if value, ok := key.Inner.Get(); ok {
			inner = cmpoKeyJSON(value)
		}
		return []any{"Tagged", map[string]any{"tag": string(key.Tag), "inner": inner}}
	}
	panic(rs.Unreachable(key))
}

// cmpoDropCounted drops the Arc handles a value holds: those of a clone,
// which the decoder did not make.
func cmpoDropCounted(value cmpoVal) {
	switch value := value.(type) {
	case *cmpoVal_Counted:
		value.F0.Drop()
	case *cmpoVal_MaybeVal:
		if inner, ok := value.F0.Get(); ok {
			cmpoDropCounted(inner)
		}
	case *cmpoVal_List:
		for _, item := range value.F0 {
			cmpoDropCounted(item)
		}
	case *cmpoVal_Triple:
		cmpoDropCounted(value.F0)
	case *cmpoVal_Rec:
		if inner, ok := value.Child.Get(); ok {
			cmpoDropCounted(inner)
		}
	case *cmpoVal_Map:
		for _, item := range value.F0.values {
			cmpoDropCounted(*item)
		}
	case *cmpoVal_Boxed:
		cmpoDropCounted(value.F0)
	}
}

// cmpoOrd writes an Option<Ordering> as std_cmp.rs does.
func cmpoOrd(order rs.Option[rs.Ordering]) any {
	if value, ok := order.Get(); ok {
		return int(value)
	}
	return nil
}

func cmpoCheck(t *testing.T, corpus string, generate oracle.Generate, answer func(request map[string]any) any) {
	t.Helper()
	oracle.Check(t, "std_cmp", corpus, generate, func(request json.RawMessage) any {
		return oracle.Catch(func() any { return answer(cmpoParse(request)) })
	})
}

func TestStdCmpOracleValues(t *testing.T) {
	cmpoCheck(t, "std_cmp_values", func(yield func(any)) {
		r := &cmpoRng{state: 1}
		for range 2500 * cmpoScale() {
			a, b := r.pair(func(g *cmpoRng) any { return g.val(3) })
			yield(map[string]any{"op": "cmp", "a": a, "b": b})
		}
	}, func(request map[string]any) any {
		left, right := &cmpoDecoder{nilEmpty: true}, &cmpoDecoder{}
		defer left.drop()
		defer right.drop()
		a, b := left.val(request["a"]), right.val(request["b"])
		clone := rs.Clone(a)
		defer cmpoDropCounted(clone)
		partial := rs.PartialCmp(a, b)
		reply := map[string]any{
			"eq":           rs.Eq(a, b),
			"partial_cmp":  cmpoOrd(partial),
			"clone_eq":     rs.Eq(clone, a),
			"same_variant": rs.Discriminant(a) == rs.Discriminant(b),
		}
		// Cmp is `partial_cmp(..).unwrap_or(Equal)`, and Eq agrees with an
		// order of Equal unless a hand-written method says otherwise.
		if got, want := rs.Cmp(a, b), int(partial.UnwrapOr(rs.Ordering_Equal)); got != want {
			reply["cmp"] = got
		}
		return reply
	})
}

func TestStdCmpOracleKeys(t *testing.T) {
	cmpoCheck(t, "std_cmp_keys", func(yield func(any)) {
		r := &cmpoRng{state: 2}
		for range 600 * cmpoScale() {
			a, b := r.pair(func(g *cmpoRng) any { return g.key(3) })
			yield(map[string]any{"op": "ord", "a": a, "b": b})
		}
	}, func(request map[string]any) any {
		left, right := &cmpoDecoder{nilEmpty: true}, &cmpoDecoder{}
		a, b := left.key(request["a"]), right.key(request["b"])
		order := rs.OrdCmp(a, b)
		reply := map[string]any{
			"eq":  rs.Eq(a, b),
			"cmp": int(order),
			"min": cmpoKeyJSON(rs.Min(a, b)),
			"max": cmpoKeyJSON(rs.Max(a, b)),
		}
		if rs.Cmp(a, b) != int(order) || rs.PartialCmp(a, b) != rs.Some(order) {
			reply["inconsistent"] = true
		}
		return reply
	})
}

func TestStdCmpOracleHash(t *testing.T) {
	cmpoCheck(t, "std_cmp_hash", func(yield func(any)) {
		r := &cmpoRng{state: 3}
		// Keys from a small pool, so that maps share keys and repeat them.
		smallKey := func(g *cmpoRng) any {
			switch g.intn(4) {
			case 0:
				return []any{"Num", g.intn(3)}
			case 1:
				return []any{"Text", cmpoPick(g, []string{"", "a", "é"})}
			case 2:
				return []any{"Pair", g.intn(2), g.intn(2) == 0}
			}
			return []any{"Nested", cmpoRepeat(g, 1, func() any { return []any{"Empty"} })}
		}
		entries := func(g *cmpoRng) any {
			return cmpoRepeat(g, 4, func() any { return []any{smallKey(g), g.val(1)} })
		}
		keys := func(g *cmpoRng) any { return cmpoRepeat(g, 4, func() any { return smallKey(g) }) }
		shuffled := func(items []any) []any {
			out := slices.Clone(items)
			for i := len(out) - 1; i > 0; i-- {
				j := r.intn(i + 1)
				out[i], out[j] = out[j], out[i]
			}
			return out
		}
		for range 400 * cmpoScale() {
			a, b := r.pair(entries)
			setA, setB := r.pair(keys)
			if r.intn(2) == 0 {
				// The same entries in another order.
				b = shuffled(a.([]any))
			}
			if r.intn(2) == 0 {
				setB = shuffled(setA.([]any))
			}
			yield(map[string]any{"op": "hash", "a": a, "b": b, "sa": setA, "sb": setB})
		}
	}, func(request map[string]any) any {
		decoder := &cmpoDecoder{}
		defer decoder.drop()
		hashMap := func(v any) cmpoHashMap[cmpoKey, cmpoVal] {
			var m cmpoHashMap[cmpoKey, cmpoVal]
			for _, entry := range v.([]any) {
				pair := entry.([]any)
				m.insert(decoder.key(pair[0]), decoder.val(pair[1]))
			}
			return m
		}
		hashSet := func(v any) cmpoHashSet[cmpoKey] {
			var s cmpoHashSet[cmpoKey]
			for _, item := range v.([]any) {
				s.insert(decoder.key(item))
			}
			return s
		}
		return map[string]any{
			"map_eq": rs.Eq(hashMap(request["a"]), hashMap(request["b"])),
			"set_eq": rs.Eq(hashSet(request["sa"]), hashSet(request["sb"])),
		}
	})
}

func TestStdCmpOracleOrdering(t *testing.T) {
	cmpoCheck(t, "std_cmp_ordering", func(yield func(any)) {
		for _, a := range []int{-1, 0, 1} {
			for _, b := range []int{-1, 0, 1} {
				yield(map[string]any{"op": "ordering", "a": a, "b": b})
			}
		}
	}, func(request map[string]any) any {
		a, b := rs.Ordering(cmpoInt(request["a"])), rs.Ordering(cmpoInt(request["b"]))
		some := func(order rs.Ordering) any { return cmpoOrd(rs.Some(order)) }
		return map[string]any{
			"reverse":     some(a.Reverse()),
			"then":        some(a.Then(b)),
			"then_with":   some(a.ThenWith(func() rs.Ordering { return b })),
			"is_eq":       a.IsEq(),
			"is_ne":       a.IsNe(),
			"is_lt":       a.IsLt(),
			"is_gt":       a.IsGt(),
			"is_le":       a.IsLe(),
			"is_ge":       a.IsGe(),
			"eq":          rs.Eq(a, b),
			"cmp":         some(rs.OrdCmp(a, b)),
			"partial_cmp": cmpoOrd(rs.PartialCmp(a, b)),
			"min":         some(rs.Min(a, b)),
			"max":         some(rs.Max(a, b)),
			"as_i8":       int8(a),
			"debug":       rs.DebugString(a),
		}
	})
}

// The types whose Default std_cmp.rs describes.

type cmpoMode uint8

const (
	cmpoMode_Off cmpoMode = iota
	cmpoMode_Push
	cmpoMode_Always
)

var _ = rs.RegisterEnum[cmpoMode]("Mode",
	rs.Const(cmpoMode_Off, "Off"),
	rs.Const(cmpoMode_Push, "Push"),
	rs.Const(cmpoMode_Always, "Always"),
)

var _ = rs.RegisterDefault(func() cmpoMode { return cmpoMode_Push })

type cmpoLayout interface{ isCmpoLayout() }

type (
	cmpoLayout_Stacked struct{}
	cmpoLayout_Split   struct {
		Ratio float64 `rs:"ratio"`
	}
)

func (*cmpoLayout_Stacked) isCmpoLayout() {}
func (*cmpoLayout_Split) isCmpoLayout()   {}

var _ = rs.RegisterEnum[cmpoLayout]("Layout",
	rs.Variant[cmpoLayout_Stacked]("Stacked", rs.ShapeUnit),
	rs.Variant[cmpoLayout_Split]("Split", rs.ShapeStruct),
)

var _ = rs.RegisterDefault(func() cmpoLayout { return &cmpoLayout_Stacked{} })

type cmpoLimit struct {
	Max   uint32   `rs:"max"`
	Hosts []string `rs:"hosts"`
}

var _ = rs.RegisterStruct[cmpoLimit]("Limit")

func cmpoLimit_default() cmpoLimit {
	return cmpoLimit{Max: 10, Hosts: []string{"localhost"}}
}

var _ = rs.RegisterDefault(cmpoLimit_default)

type cmpoLeaf struct {
	Mode cmpoMode `rs:"mode"`
	Name string   `rs:"name"`
}

var _ = rs.RegisterStruct[cmpoLeaf]("Leaf")

type cmpoSettings struct {
	Mode    cmpoMode                        `rs:"mode"`
	Layout  cmpoLayout                      `rs:"layout"`
	Limit   cmpoLimit                       `rs:"limit"`
	Boxed   *cmpoLeaf                       `rs:"boxed"`
	Shared  *cmpoLeaf                       `rs:"shared,arc"`
	Counted rs.Arc[cmpoLeaf]                `rs:"counted"`
	Maybe   rs.Option[*cmpoLeaf]            `rs:"maybe"`
	Names   []string                        `rs:"names"`
	Lookup  map[string]int32                `rs:"lookup"`
	Tree    cmpoTreeMap[string, cmpoMode]   `rs:"tree"`
	Pair    rs.Tuple2[cmpoMode, *cmpoLimit] `rs:"pair"`
	Modes   [2]cmpoMode                     `rs:"modes"`
	Scroll  rs.Cell[cmpoMode]               `rs:"scroll"`
	Areas   rs.RefCell[[]cmpoMode]          `rs:"areas"`
	Rev     rs.Reverse[cmpoMode]            `rs:"rev"`
	Count   uint64                          `rs:"count"`
	Ratio   float64                         `rs:"ratio"`
	Flag    bool                            `rs:"flag"`
	Letter  rune                            `rs:"letter"`
	Title   string                          `rs:"title"`
	Unit    rs.Unit                         `rs:"unit"`
	Nested  rs.Option[cmpoLimit]            `rs:"nested"`
}

var _ = rs.RegisterStruct[cmpoSettings]("Settings")

// Describing values as std_cmp.rs's Describe does. A nil pointer, map or
// interface, which Rust never has, is "<nil>".

const cmpoNil = "<nil>"

func cmpoDescribeList[T any](items []T, describe func(T) any) any {
	out := make([]any, 0, len(items))
	for _, item := range items {
		out = append(out, describe(item))
	}
	return out
}

func cmpoDescribeOption[T any](option rs.Option[T], describe func(T) any) any {
	if value, ok := option.Get(); ok {
		return map[string]any{"Some": describe(value)}
	}
	return "None"
}

func cmpoDescribeBox[T any](box *T, describe func(T) any) any {
	if box == nil {
		return cmpoNil
	}
	return describe(*box)
}

func cmpoDescribeArc[T any](arc rs.Arc[T], describe func(T) any) any {
	return describe(*arc.Get())
}

func cmpoDescribeString(s string) any { return s }

func cmpoDescribeFloat(f float64) any { return math.Float64bits(f) }

func cmpoDescribeMode(mode cmpoMode) any {
	if variant := rs.VariantOf(mode); variant != nil {
		return variant.RustName
	}
	return "<not a Mode>"
}

func cmpoDescribeLayout(layout cmpoLayout) any {
	switch layout := layout.(type) {
	case *cmpoLayout_Stacked:
		return "Stacked"
	case *cmpoLayout_Split:
		return map[string]any{"Split": map[string]any{"ratio": cmpoDescribeFloat(layout.Ratio)}}
	}
	return cmpoNil
}

func cmpoDescribeLimit(limit cmpoLimit) any {
	return map[string]any{"max": limit.Max, "hosts": cmpoDescribeList(limit.Hosts, cmpoDescribeString)}
}

func cmpoDescribeLeaf(leaf cmpoLeaf) any {
	return map[string]any{"mode": cmpoDescribeMode(leaf.Mode), "name": leaf.Name}
}

func cmpoDescribeMap[V any](m map[string]V, describe func(V) any) any {
	if m == nil {
		return cmpoNil
	}
	out := map[string]any{}
	for key, value := range m {
		out[key] = describe(value)
	}
	return out
}

func cmpoDescribeTree[V any](m cmpoTreeMap[string, V], describe func(V) any) any {
	out := map[string]any{}
	for at, key := range m.keys {
		out[key] = describe(*m.values[at])
	}
	return out
}

func cmpoDescribeSettings(s cmpoSettings) any {
	return map[string]any{
		"mode":    cmpoDescribeMode(s.Mode),
		"layout":  cmpoDescribeLayout(s.Layout),
		"limit":   cmpoDescribeLimit(s.Limit),
		"boxed":   cmpoDescribeBox(s.Boxed, cmpoDescribeLeaf),
		"shared":  cmpoDescribeBox(s.Shared, cmpoDescribeLeaf),
		"counted": cmpoDescribeArc(s.Counted, cmpoDescribeLeaf),
		"maybe": cmpoDescribeOption(s.Maybe, func(leaf *cmpoLeaf) any {
			return cmpoDescribeBox(leaf, cmpoDescribeLeaf)
		}),
		"names":  cmpoDescribeList(s.Names, cmpoDescribeString),
		"lookup": cmpoDescribeMap(s.Lookup, func(n int32) any { return n }),
		"tree":   cmpoDescribeTree(s.Tree, cmpoDescribeMode),
		"pair":   []any{cmpoDescribeMode(s.Pair.F0), cmpoDescribeBox(s.Pair.F1, cmpoDescribeLimit)},
		"modes":  cmpoDescribeList(s.Modes[:], cmpoDescribeMode),
		"scroll": cmpoDescribeMode(s.Scroll.Get()),
		"areas":  cmpoDescribeList(*s.Areas.Borrow(), cmpoDescribeMode),
		"rev":    cmpoDescribeMode(s.Rev.F0),
		"count":  s.Count,
		"ratio":  cmpoDescribeFloat(s.Ratio),
		"flag":   s.Flag,
		"letter": string(s.Letter),
		"title":  s.Title,
		"unit":   "()",
		"nested": cmpoDescribeOption(s.Nested, cmpoDescribeLimit),
	}
}

// cmpoDefaults describes rs.Default of each type std_cmp.rs knows by name.
var cmpoDefaults = map[string]func() any{
	"Settings": func() any {
		settings := rs.Default[cmpoSettings]()
		defer settings.Counted.Drop()
		return cmpoDescribeSettings(settings)
	},
	"Mode":   func() any { return cmpoDescribeMode(rs.Default[cmpoMode]()) },
	"Layout": func() any { return cmpoDescribeLayout(rs.Default[cmpoLayout]()) },
	"Limit":  func() any { return cmpoDescribeLimit(rs.Default[cmpoLimit]()) },
	"Leaf":   func() any { return cmpoDescribeLeaf(rs.Default[cmpoLeaf]()) },
	"Option<Box<Leaf>>": func() any {
		return cmpoDescribeOption(rs.Default[rs.Option[*cmpoLeaf]](), func(leaf *cmpoLeaf) any {
			return cmpoDescribeBox(leaf, cmpoDescribeLeaf)
		})
	},
	"Option<Limit>": func() any { return cmpoDescribeOption(rs.Default[rs.Option[cmpoLimit]](), cmpoDescribeLimit) },
	"Box<Leaf>":     func() any { return cmpoDescribeBox(rs.Default[*cmpoLeaf](), cmpoDescribeLeaf) },
	"Arc<Leaf>": func() any {
		arc := rs.Default[rs.Arc[cmpoLeaf]]()
		defer arc.Drop()
		return cmpoDescribeArc(arc, cmpoDescribeLeaf)
	},
	"(Mode, Box<Limit>)": func() any {
		pair := rs.Default[rs.Tuple2[cmpoMode, *cmpoLimit]]()
		return []any{cmpoDescribeMode(pair.F0), cmpoDescribeBox(pair.F1, cmpoDescribeLimit)}
	},
	"[Mode; 3]": func() any {
		modes := rs.Default[[3]cmpoMode]()
		return cmpoDescribeList(modes[:], cmpoDescribeMode)
	},
	"[Limit; 2]": func() any {
		limits := rs.Default[[2]cmpoLimit]()
		return cmpoDescribeList(limits[:], cmpoDescribeLimit)
	},
	"Cell<Mode>": func() any {
		cell := rs.Default[rs.Cell[cmpoMode]]()
		return cmpoDescribeMode(cell.Get())
	},
	"RefCell<Vec<Mode>>": func() any {
		cell := rs.Default[rs.RefCell[[]cmpoMode]]()
		return cmpoDescribeList(*cell.Borrow(), cmpoDescribeMode)
	},
	"Reverse<Mode>": func() any { return cmpoDescribeMode(rs.Default[rs.Reverse[cmpoMode]]().F0) },
	"Vec<Mode>":     func() any { return cmpoDescribeList(rs.Default[[]cmpoMode](), cmpoDescribeMode) },
	"HashMap<String, Mode>": func() any {
		return cmpoDescribeMap(rs.Default[map[string]cmpoMode](), cmpoDescribeMode)
	},
	"BTreeMap<String, Limit>": func() any {
		return cmpoDescribeTree(rs.Default[cmpoTreeMap[string, cmpoLimit]](), cmpoDescribeLimit)
	},
	"char":   func() any { return string(rs.Default[rune]()) },
	"f64":    func() any { return cmpoDescribeFloat(rs.Default[float64]()) },
	"u64":    func() any { return rs.Default[uint64]() },
	"bool":   func() any { return rs.Default[bool]() },
	"String": func() any { return rs.Default[string]() },
	"()": func() any {
		_ = rs.Default[rs.Unit]()
		return "()"
	},
}

func TestStdCmpOracleDefault(t *testing.T) {
	cmpoCheck(t, "std_cmp_default", func(yield func(any)) {
		names := make([]string, 0, len(cmpoDefaults))
		for name := range cmpoDefaults {
			names = append(names, name)
		}
		slices.Sort(names)
		for _, name := range names {
			yield(map[string]any{"op": "default", "type": name})
		}
	}, func(request map[string]any) any {
		describe, found := cmpoDefaults[request["type"].(string)]
		if !found {
			return map[string]any{"unknown": request["type"]}
		}
		return map[string]any{"value": describe()}
	})
}
