// Port of core 1.99.1 src/cmp.rs (Ordering, Reverse, and PartialOrd and Ord
// as #[derive(PartialOrd, Ord)] generates them, done by reflection)

package rs

import (
	"bytes"
	"cmp"
	"reflect"
	"slices"
	"unsafe"
)

// How two values are ordered.
//
// Cmp follows the derived Ord of the Rust type a Go type stands for (clone.go
// describes how Go types are read):
//
//   - Numbers compare by value, bools with false first, strings byte by byte.
//     A data-free enum compares by its discriminant, which is its Go value.
//   - Slices and arrays compare element by element; when one is a prefix of
//     the other, the shorter one is less.
//   - Tuples and structs compare field by field in declaration order,
//     unexported fields included.
//   - None is less than Some, and Ok less than Err.
//   - A pointer is ordered by what it points to.
//   - Two variants of one enum are ordered by their position in the
//     RegisterEnum call; two values of the same variant by their fields.
//   - Ordered collections (BTreeMap, BTreeSet, VecDeque) compare entry by
//     entry in iteration order, as slices do.
//   - A type with a method `Compare(other T) int` or `Compare(other *T) int`
//     is ordered by it wherever it appears.
//
// Floats have only a partial order: a NaN is neither less than, equal to nor
// greater than anything. PartialCmp reports that as None, exactly as
// `partial_cmp` does, at whatever depth the NaN sits. Cmp has no such answer
// to give and returns 0 for it, which is `partial_cmp(..).unwrap_or(Equal)`:
// `Cmp(a, b) < 0` and `Cmp(a, b) > 0` are then false, as `a < b` and `a > b`
// are in Rust, but `Cmp(a, b) <= 0` is true where Rust's `a <= b` is false.
//
// Rust has no order for a HashMap, a HashSet, a func or a channel, and none
// between values that are not variants of one enum. Cmp orders maps and sets
// by their sorted entries and funcs and channels by address, so that it is
// total, and panics for two values of unrelated types held in one interface.

// Ordering is core::cmp::Ordering. It holds -1, 0 or 1, as the Rust enum
// does, and Cmp returns the same three numbers as an int: convert with
// OrderingOf, or call OrdCmp for an Ordering to begin with.
type Ordering int8

const (
	// Ordering_Less is `Ordering::Less`.
	Ordering_Less Ordering = -1
	// Ordering_Equal is `Ordering::Equal`.
	Ordering_Equal Ordering = 0
	// Ordering_Greater is `Ordering::Greater`.
	Ordering_Greater Ordering = 1
)

var _ = RegisterEnum[Ordering]("Ordering",
	Const(Ordering_Less, "Less"),
	Const(Ordering_Equal, "Equal"),
	Const(Ordering_Greater, "Greater"),
)

// OrderingOf turns the result of Cmp, of a Compare method or of any other
// comparison that answers negative, zero or positive into an Ordering.
func OrderingOf(order int) Ordering {
	return Ordering(sign(int64(order)))
}

// IsEq is Ordering::is_eq.
func (o Ordering) IsEq() bool { return o == 0 }

// IsNe is Ordering::is_ne.
func (o Ordering) IsNe() bool { return o != 0 }

// IsLt is Ordering::is_lt.
func (o Ordering) IsLt() bool { return o < 0 }

// IsGt is Ordering::is_gt.
func (o Ordering) IsGt() bool { return o > 0 }

// IsLe is Ordering::is_le.
func (o Ordering) IsLe() bool { return o <= 0 }

// IsGe is Ordering::is_ge.
func (o Ordering) IsGe() bool { return o >= 0 }

// Reverse is Ordering::reverse.
func (o Ordering) Reverse() Ordering {
	switch {
	case o < 0:
		return Ordering_Greater
	case o > 0:
		return Ordering_Less
	}
	return Ordering_Equal
}

// Then is Ordering::then.
func (o Ordering) Then(other Ordering) Ordering {
	if o == Ordering_Equal {
		return other
	}
	return o
}

// ThenWith is Ordering::then_with: f is called only when o is Equal.
func (o Ordering) ThenWith(f func() Ordering) Ordering {
	if o == Ordering_Equal {
		return f()
	}
	return o
}

// Reverse is core::cmp::Reverse, the wrapper that turns an order round:
// `sort_by_key(|entry| Reverse(entry.score))`. Equality is that of the value
// inside.
type Reverse[T any] struct {
	F0 T
}

var _ = RegisterTupleStruct[Reverse[int]]("Reverse")

// OrdCmp is `Ord::cmp(&a, &b)` with Rust's result type. It is Cmp for the
// places that keep the Ordering: a field, a `match`, a `then_with` chain.
func OrdCmp[T any](a, b T) Ordering {
	return Ordering(compareValues(a, b))
}

// PartialCmp is `a.partial_cmp(&b)`: None when the two cannot be ordered,
// which only a float NaN somewhere inside them causes.
func PartialCmp[T any](a, b T) Option[Ordering] {
	order, ok := partialCompareValues(a, b)
	if !ok {
		return None[Ordering]()
	}
	return Some(Ordering(order))
}

// compareValues is the body of Cmp. The result is always -1, 0 or 1.
func compareValues[T any](a, b T) int {
	order, _ := partialCompareValues(a, b)
	return order
}

// partialCompareValues orders a and b, and reports false with 0 when they
// cannot be ordered.
func partialCompareValues[T any](a, b T) (int, bool) {
	t := reflect.TypeFor[T]()
	switch t.Kind() {
	case reflect.String, reflect.Bool,
		reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		if order, ok := compareOrdered(any(a), any(b)); ok {
			return order, true
		}
	}
	if comparer, ok := any(&a).(interface{ Compare(T) int }); ok {
		return sign(int64(comparer.Compare(b))), true
	}
	if comparer, ok := any(&a).(interface{ Compare(*T) int }); ok {
		return sign(int64(comparer.Compare(&b))), true
	}
	return shapeOf(t).compareAt(unsafe.Pointer(&a), unsafe.Pointer(&b))
}

func sign(order int64) int {
	switch {
	case order < 0:
		return -1
	case order > 0:
		return 1
	}
	return 0
}

// partialCompareFloat is `partial_cmp` of the float types.
func partialCompareFloat(a, b float64) (int, bool) {
	switch {
	case a < b:
		return -1, true
	case a > b:
		return 1, true
	case a == b:
		return 0, true
	}
	return 0, false
}

func compareBool(a, b bool) int {
	switch {
	case a == b:
		return 0
	case !a:
		return -1
	}
	return 1
}

// compareAt orders the values at a and b, which both point to memory of the
// shape's type. It reports false with 0 when they cannot be ordered.
func (s *shape) compareAt(a, b unsafe.Pointer) (int, bool) {
	if s.compare.ok {
		return s.callCompare(a, b), true
	}
	switch s.kind {
	case kindBool:
		return compareBool(*(*bool)(a), *(*bool)(b)), true
	case kindInt:
		switch s.size {
		case 1:
			return cmp.Compare(*(*int8)(a), *(*int8)(b)), true
		case 2:
			return cmp.Compare(*(*int16)(a), *(*int16)(b)), true
		case 4:
			return cmp.Compare(*(*int32)(a), *(*int32)(b)), true
		default:
			return cmp.Compare(*(*int64)(a), *(*int64)(b)), true
		}
	case kindUint:
		switch s.size {
		case 1:
			return cmp.Compare(*(*uint8)(a), *(*uint8)(b)), true
		case 2:
			return cmp.Compare(*(*uint16)(a), *(*uint16)(b)), true
		case 4:
			return cmp.Compare(*(*uint32)(a), *(*uint32)(b)), true
		default:
			return cmp.Compare(*(*uint64)(a), *(*uint64)(b)), true
		}
	case kindFloat:
		if s.size == 4 {
			return partialCompareFloat(float64(*(*float32)(a)), float64(*(*float32)(b)))
		}
		return partialCompareFloat(*(*float64)(a), *(*float64)(b))
	case kindComplex:
		var left, right complex128
		if s.size == 8 {
			left, right = complex128(*(*complex64)(a)), complex128(*(*complex64)(b))
		} else {
			left, right = *(*complex128)(a), *(*complex128)(b)
		}
		if order, ok := partialCompareFloat(real(left), real(right)); !ok || order != 0 {
			return order, ok
		}
		return partialCompareFloat(imag(left), imag(right))
	case kindString:
		return cmp.Compare(*(*string)(a), *(*string)(b)), true
	case kindHandle:
		return cmp.Compare(uintptr(*(*unsafe.Pointer)(a)), uintptr(*(*unsafe.Pointer)(b))), true
	case kindPointer:
		return s.elem.comparePointees(*(*unsafe.Pointer)(a), *(*unsafe.Pointer)(b))
	case kindInterface:
		return s.compareInterface(a, b)
	case kindSlice:
		left, right := reflect.NewAt(s.typ, a).Elem(), reflect.NewAt(s.typ, b).Elem()
		return s.elem.compareRuns(left.UnsafePointer(), left.Len(), right.UnsafePointer(), right.Len())
	case kindArray:
		return s.elem.compareRuns(a, s.typ.Len(), b, s.typ.Len())
	case kindGoMap:
		return s.compareGoMap(a, b)
	case kindStruct:
		for _, field := range s.fields {
			left, right := unsafe.Add(a, field.offset), unsafe.Add(b, field.offset)
			if s.reversed {
				// Reverse: `other.0.partial_cmp(&self.0)`.
				left, right = right, left
			}
			if order, ok := field.shape.compareAt(left, right); !ok || order != 0 {
				return order, ok
			}
		}
		return 0, true
	case kindOption:
		some := s.fields[1].offset
		left, right := *(*bool)(unsafe.Add(a, some)), *(*bool)(unsafe.Add(b, some))
		if !left || !right {
			return compareBool(left, right), true
		}
		value := s.fields[0]
		return value.shape.compareAt(unsafe.Add(a, value.offset), unsafe.Add(b, value.offset))
	case kindResult:
		value, err := s.fields[0], s.fields[1]
		left, right := *(*error)(unsafe.Add(a, err.offset)), *(*error)(unsafe.Add(b, err.offset))
		if left == nil || right == nil {
			if left == nil && right == nil {
				return value.shape.compareAt(unsafe.Add(a, value.offset), unsafe.Add(b, value.offset))
			}
			// Ok is declared before Err.
			return compareBool(left != nil, right != nil), true
		}
		return err.shape.compareAt(unsafe.Add(a, err.offset), unsafe.Add(b, err.offset))
	case kindReflectMap:
		return s.compareReflectMap(a, b)
	case kindReflectSeq:
		return s.compareReflectSeq(a, b)
	}
	panic("rs.Cmp: unhandled kind of " + s.typ.String())
}

// callCompare runs the type's hand-written Compare on the values at a and b.
func (s *shape) callCompare(a, b unsafe.Pointer) int {
	argument := reflect.NewAt(s.typ, b)
	if s.compare.form == formValue {
		argument = argument.Elem()
	}
	return sign(reflect.NewAt(s.typ, a).Method(s.compare.index).Call([]reflect.Value{argument})[0].Int())
}

// comparePointees orders two pointers to values of the shape's type by what
// they point to.
func (s *shape) comparePointees(a, b unsafe.Pointer) (int, bool) {
	if a == nil || b == nil {
		// Rust has no nil; a missing value sorts first.
		return compareBool(a != nil, b != nil), true
	}
	if s.compare.ok {
		return s.callCompare(a, b), true
	}
	if s.pinned {
		return cmp.Compare(uintptr(a), uintptr(b)), true
	}
	if a == b && s.isReflexive() {
		return 0, true
	}
	return s.compareAt(a, b)
}

// compareRuns orders two runs of consecutive values of the shape's type as
// slices are ordered: by the first element that differs, then by length.
func (s *shape) compareRuns(a unsafe.Pointer, aCount int, b unsafe.Pointer, bCount int) (int, bool) {
	if !s.compare.ok && s.kind == kindUint && s.size == 1 {
		return bytes.Compare(unsafe.Slice((*byte)(a), aCount), unsafe.Slice((*byte)(b), bCount)), true
	}
	for i := range min(aCount, bCount) {
		offset := uintptr(i) * s.size
		if order, ok := s.compareAt(unsafe.Add(a, offset), unsafe.Add(b, offset)); !ok || order != 0 {
			return order, ok
		}
	}
	return cmp.Compare(aCount, bCount), true
}

func (s *shape) compareInterface(a, b unsafe.Pointer) (int, bool) {
	left, right := reflect.NewAt(s.typ, a).Elem(), reflect.NewAt(s.typ, b).Elem()
	if left.IsNil() || right.IsNil() {
		return compareBool(!left.IsNil(), !right.IsNil()), true
	}
	leftHeld, rightHeld := left.Elem(), right.Elem()
	leftType, rightType := leftHeld.Type(), rightHeld.Type()
	if leftType.Kind() == reflect.Pointer {
		pointee := shapeOf(leftType.Elem())
		if method := pointee.compare; method.ok && method.form == formInterface && !leftHeld.IsNil() &&
			rightType.Implements(method.param) {
			// An enum's hand-written Ord: it decides for every pair of
			// variants.
			return sign(leftHeld.Method(method.index).Call([]reflect.Value{rightHeld})[0].Int()), true
		}
		if leftType == rightType {
			return pointee.comparePointees(leftHeld.UnsafePointer(), rightHeld.UnsafePointer())
		}
	} else if leftType == rightType {
		return shapeOf(leftType).compareHeld(leftHeld, rightHeld)
	}
	leftVariant, rightVariant := heldVariantInfo(leftType), heldVariantInfo(rightType)
	if leftVariant == nil || rightVariant == nil || leftVariant.Enum != rightVariant.Enum {
		panic("rs.Cmp: cannot order a " + leftType.String() + " and a " + rightType.String() + " held in a " +
			s.typ.String() + ": they are not variants of one registered enum")
	}
	return cmp.Compare(leftVariant.Index, rightVariant.Index), true
}

// heldVariantInfo returns the registration of the enum variant that a value
// of type t held in an interface is, or nil.
func heldVariantInfo(t reflect.Type) *TypeInfo {
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	return enumVariantInfo(t)
}

// compareHeld orders two values of the shape's type that are held in
// interfaces or came out of a Go map, where reflection cannot take their
// address.
func (s *shape) compareHeld(a, b reflect.Value) (int, bool) {
	if !s.compare.ok {
		switch s.kind {
		case kindBool:
			return compareBool(a.Bool(), b.Bool()), true
		case kindInt:
			return cmp.Compare(a.Int(), b.Int()), true
		case kindUint:
			return cmp.Compare(a.Uint(), b.Uint()), true
		case kindFloat:
			return partialCompareFloat(a.Float(), b.Float())
		case kindString:
			return cmp.Compare(a.String(), b.String()), true
		}
	}
	left, right := reflect.New(s.typ), reflect.New(s.typ)
	left.Elem().Set(a)
	right.Elem().Set(b)
	return s.compareAt(left.UnsafePointer(), right.UnsafePointer())
}

// compareBoxed orders two values of the shape's type that a collection
// handed over in interfaces.
func (s *shape) compareBoxed(a, b any) (int, bool) {
	if s.kind != kindInterface && a != nil && b != nil {
		return s.compareHeld(reflect.ValueOf(a), reflect.ValueOf(b))
	}
	return s.compareAt(s.boxedAddress(a), s.boxedAddress(b))
}

// compareGoMap orders two Go maps by their entries in key order, as two
// BTreeMaps are ordered.
func (s *shape) compareGoMap(a, b unsafe.Pointer) (int, bool) {
	left, right := reflect.NewAt(s.typ, a).Elem(), reflect.NewAt(s.typ, b).Elem()
	sortedKeys := func(m reflect.Value) []reflect.Value {
		keys := m.MapKeys()
		slices.SortFunc(keys, func(x, y reflect.Value) int {
			order, _ := s.key.compareHeld(x, y)
			return order
		})
		return keys
	}
	leftKeys, rightKeys := sortedKeys(left), sortedKeys(right)
	leftValue, rightValue := reflect.New(s.elem.typ), reflect.New(s.elem.typ)
	for i := range min(len(leftKeys), len(rightKeys)) {
		if order, ok := s.key.compareHeld(leftKeys[i], rightKeys[i]); !ok || order != 0 {
			return order, ok
		}
		leftValue.Elem().Set(left.MapIndex(leftKeys[i]))
		rightValue.Elem().Set(right.MapIndex(rightKeys[i]))
		if order, ok := s.elem.compareAt(leftValue.UnsafePointer(), rightValue.UnsafePointer()); !ok || order != 0 {
			return order, ok
		}
	}
	return cmp.Compare(len(leftKeys), len(rightKeys)), true
}

// compareReflectMap is BTreeMap's `self.iter().cmp(other.iter())`: entry by
// entry, each a (key, value) pair, then by length.
func (s *shape) compareReflectMap(a, b unsafe.Pointer) (int, bool) {
	left := reflect.NewAt(s.typ, a).Interface().(ReflectMap)
	right := reflect.NewAt(s.typ, b).Interface().(ReflectMap)
	sortedEntries := func(collection ReflectMap) []reflectEntry {
		entries := s.reflectEntries(collection)
		if !collection.ReflectSorted() {
			slices.SortStableFunc(entries, func(x, y reflectEntry) int {
				order, _ := s.key.compareBoxed(x.key, y.key)
				return order
			})
		}
		return entries
	}
	leftEntries, rightEntries := sortedEntries(left), sortedEntries(right)
	for i := range min(len(leftEntries), len(rightEntries)) {
		if order, ok := s.key.compareBoxed(leftEntries[i].key, rightEntries[i].key); !ok || order != 0 {
			return order, ok
		}
		if order, ok := s.elem.compareAt(leftEntries[i].value, rightEntries[i].value); !ok || order != 0 {
			return order, ok
		}
	}
	return cmp.Compare(len(leftEntries), len(rightEntries)), true
}

// compareReflectSeq is `self.iter().cmp(other.iter())` of VecDeque and
// BTreeSet.
func (s *shape) compareReflectSeq(a, b unsafe.Pointer) (int, bool) {
	left := reflect.NewAt(s.typ, a).Interface().(ReflectSeq)
	right := reflect.NewAt(s.typ, b).Interface().(ReflectSeq)
	sortedElements := func(collection ReflectSeq) []unsafe.Pointer {
		elements := s.reflectElements(collection)
		if collection.ReflectIsSet() && !collection.ReflectSorted() {
			slices.SortStableFunc(elements, func(x, y unsafe.Pointer) int {
				order, _ := s.elem.compareAt(x, y)
				return order
			})
		}
		return elements
	}
	leftElements, rightElements := sortedElements(left), sortedElements(right)
	for i := range min(len(leftElements), len(rightElements)) {
		if order, ok := s.elem.compareAt(leftElements[i], rightElements[i]); !ok || order != 0 {
			return order, ok
		}
	}
	return cmp.Compare(len(leftElements), len(rightElements)), true
}
