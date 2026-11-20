// Port of core 1.99.1 src/cmp.rs (PartialEq, as #[derive(PartialEq)]
// generates it, done by reflection)

package rs

import (
	"bytes"
	"reflect"
	"unsafe"
)

// How two values are compared for equality.
//
// Eq follows the derived PartialEq of the Rust type a Go type stands for
// (clone.go describes how Go types are read):
//
//   - Numbers, bools and strings compare with ==. A float NaN is not equal to
//     itself.
//   - Slices are equal when they have the same length and equal elements, so
//     a nil slice equals an empty one. Arrays, tuples and structs compare
//     field by field, unexported fields included.
//   - An Option is equal to another when both are None or both hold equal
//     values; a Result when both are Ok with equal values or both Err with
//     equal errors.
//   - A pointer is compared by what it points to, as Box<T> and Arc<T> are,
//     and so is an rs.Arc.
//   - Two interface values are equal when they hold the same type and equal
//     values: for a Rust enum, the same variant with equal fields.
//   - Go maps, and collections whose iteration order is not their key order
//     (HashMap, HashSet, IndexMap), are equal when they have the same length
//     and every key of one is in the other with an equal value. Ordered
//     collections and sequences (BTreeMap, BTreeSet, VecDeque) are compared
//     element by element in iteration order.
//   - A type with a method `Equal(other T) bool` or `Equal(other *T) bool` is
//     compared by it wherever it appears.
//   - Funcs and channels are equal when they are the same func value or
//     channel. So are two pointers to a type that Clone never copies (a lock,
//     a NoReflectClone type): Rust cannot compare those at all.
//
// Rust skips the comparison of two Arc handles to one allocation only when
// T: Eq. Go cannot see that marker, so two pointers to the same value are
// taken as equal without looking only when the type's shape proves that
// equality is reflexive: no float anywhere below it and no hand-written
// Equal.

// equalValues is the body of Eq.
func equalValues[T any](a, b T) bool {
	t := reflect.TypeFor[T]()
	switch t.Kind() {
	case reflect.String, reflect.Bool,
		reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		if order, ok := compareOrdered(any(a), any(b)); ok {
			return order == 0
		}
	}
	if equaler, ok := any(&a).(interface{ Equal(T) bool }); ok {
		return equaler.Equal(b)
	}
	if equaler, ok := any(&a).(interface{ Equal(*T) bool }); ok {
		return equaler.Equal(&b)
	}
	return shapeOf(t).equalAt(unsafe.Pointer(&a), unsafe.Pointer(&b))
}

// equalAt compares the values at a and b, which both point to memory of the
// shape's type.
func (s *shape) equalAt(a, b unsafe.Pointer) bool {
	if s.equal.ok {
		return s.callEqual(a, b)
	}
	switch s.kind {
	case kindBool, kindInt, kindUint:
		switch s.size {
		case 1:
			return *(*uint8)(a) == *(*uint8)(b)
		case 2:
			return *(*uint16)(a) == *(*uint16)(b)
		case 4:
			return *(*uint32)(a) == *(*uint32)(b)
		default:
			return *(*uint64)(a) == *(*uint64)(b)
		}
	case kindFloat:
		if s.size == 4 {
			return *(*float32)(a) == *(*float32)(b)
		}
		return *(*float64)(a) == *(*float64)(b)
	case kindComplex:
		if s.size == 8 {
			return *(*complex64)(a) == *(*complex64)(b)
		}
		return *(*complex128)(a) == *(*complex128)(b)
	case kindString:
		return *(*string)(a) == *(*string)(b)
	case kindHandle:
		return *(*unsafe.Pointer)(a) == *(*unsafe.Pointer)(b)
	case kindPointer:
		return s.elem.equalPointees(*(*unsafe.Pointer)(a), *(*unsafe.Pointer)(b))
	case kindInterface:
		return s.equalInterface(a, b)
	case kindSlice:
		left, right := reflect.NewAt(s.typ, a).Elem(), reflect.NewAt(s.typ, b).Elem()
		length := left.Len()
		if length != right.Len() {
			return false
		}
		if length == 0 {
			return true
		}
		return s.elem.equalRun(left.UnsafePointer(), right.UnsafePointer(), length)
	case kindArray:
		return s.elem.equalRun(a, b, s.typ.Len())
	case kindGoMap:
		return s.equalGoMap(a, b)
	case kindStruct:
		for _, field := range s.fields {
			if !field.shape.equalAt(unsafe.Add(a, field.offset), unsafe.Add(b, field.offset)) {
				return false
			}
		}
		return true
	case kindOption:
		some := s.fields[1].offset
		left, right := *(*bool)(unsafe.Add(a, some)), *(*bool)(unsafe.Add(b, some))
		if !left || !right {
			return left == right
		}
		value := s.fields[0]
		return value.shape.equalAt(unsafe.Add(a, value.offset), unsafe.Add(b, value.offset))
	case kindResult:
		value, err := s.fields[0], s.fields[1]
		left, right := *(*error)(unsafe.Add(a, err.offset)), *(*error)(unsafe.Add(b, err.offset))
		if (left == nil) != (right == nil) {
			return false
		}
		if left == nil {
			return value.shape.equalAt(unsafe.Add(a, value.offset), unsafe.Add(b, value.offset))
		}
		return err.shape.equalAt(unsafe.Add(a, err.offset), unsafe.Add(b, err.offset))
	case kindReflectMap:
		return s.equalReflectMap(a, b)
	case kindReflectSeq:
		return s.equalReflectSeq(a, b)
	case kindArc:
		return s.elem.equalPointees(s.arcPointee(a), s.arcPointee(b))
	}
	panic("rs.Eq: unhandled kind of " + s.typ.String())
}

// callEqual runs the type's hand-written Equal on the values at a and b.
func (s *shape) callEqual(a, b unsafe.Pointer) bool {
	argument := reflect.NewAt(s.typ, b)
	if s.equal.form == formValue {
		argument = argument.Elem()
	}
	return reflect.NewAt(s.typ, a).Method(s.equal.index).Call([]reflect.Value{argument})[0].Bool()
}

// equalPointees compares two pointers to values of the shape's type the way
// Box<T> and Arc<T> compare: by what they point to.
func (s *shape) equalPointees(a, b unsafe.Pointer) bool {
	if a == nil || b == nil {
		return a == b
	}
	if s.equal.ok {
		return s.callEqual(a, b)
	}
	if s.pinned {
		return a == b
	}
	if a == b && s.isReflexive() {
		return true
	}
	return s.equalAt(a, b)
}

// equalRun compares count consecutive values of the shape's type.
func (s *shape) equalRun(a, b unsafe.Pointer, count int) bool {
	if count == 0 || s.size == 0 && !s.equal.ok {
		return true
	}
	if a == b && s.isReflexive() {
		return true
	}
	if !s.equal.ok && (s.kind == kindBool || s.kind == kindInt || s.kind == kindUint) {
		// Integers are equal exactly when their bytes are.
		size := count * int(s.size)
		return bytes.Equal(unsafe.Slice((*byte)(a), size), unsafe.Slice((*byte)(b), size))
	}
	for i := range count {
		offset := uintptr(i) * s.size
		if !s.equalAt(unsafe.Add(a, offset), unsafe.Add(b, offset)) {
			return false
		}
	}
	return true
}

func (s *shape) equalInterface(a, b unsafe.Pointer) bool {
	left, right := reflect.NewAt(s.typ, a).Elem(), reflect.NewAt(s.typ, b).Elem()
	if left.IsNil() || right.IsNil() {
		return left.IsNil() && right.IsNil()
	}
	leftHeld, rightHeld := left.Elem(), right.Elem()
	leftType, rightType := leftHeld.Type(), rightHeld.Type()
	if leftType.Kind() != reflect.Pointer {
		if leftType != rightType {
			return false
		}
		return shapeOf(leftType).equalHeld(leftHeld, rightHeld)
	}
	pointee := shapeOf(leftType.Elem())
	if method := pointee.equal; method.ok && method.form == formInterface && !leftHeld.IsNil() &&
		rightType.Implements(method.param) {
		// An enum's hand-written PartialEq: it decides for every pair of
		// variants.
		return leftHeld.Method(method.index).Call([]reflect.Value{rightHeld})[0].Bool()
	}
	if leftType != rightType {
		return false
	}
	return pointee.equalPointees(leftHeld.UnsafePointer(), rightHeld.UnsafePointer())
}

// equalHeld compares two values of the shape's type that are held in
// interfaces, where reflection cannot take their address.
func (s *shape) equalHeld(a, b reflect.Value) bool {
	if !s.equal.ok {
		switch s.kind {
		case kindBool:
			return a.Bool() == b.Bool()
		case kindInt:
			return a.Int() == b.Int()
		case kindUint:
			return a.Uint() == b.Uint()
		case kindFloat:
			return a.Float() == b.Float()
		case kindComplex:
			return a.Complex() == b.Complex()
		case kindString:
			return a.String() == b.String()
		}
	}
	left, right := reflect.New(s.typ), reflect.New(s.typ)
	left.Elem().Set(a)
	right.Elem().Set(b)
	return s.equalAt(left.UnsafePointer(), right.UnsafePointer())
}

// equalBoxed compares two values of the shape's type that a collection
// handed over in interfaces.
func (s *shape) equalBoxed(a, b any) bool {
	if s.kind != kindInterface && a != nil && b != nil {
		return s.equalHeld(reflect.ValueOf(a), reflect.ValueOf(b))
	}
	return s.equalAt(s.boxedAddress(a), s.boxedAddress(b))
}

// boxedAddress stores a value of the shape's type that arrived in an
// interface where its address can be taken.
func (s *shape) boxedAddress(value any) unsafe.Pointer {
	slot := reflect.New(s.typ)
	if value != nil {
		slot.Elem().Set(reflect.ValueOf(value))
	}
	return slot.UnsafePointer()
}

// pointeeAddress returns the address a *T handed over in an interface points
// to, T being the shape's type.
func (s *shape) pointeeAddress(pointer any) unsafe.Pointer {
	value := reflect.ValueOf(pointer)
	if value.Kind() != reflect.Pointer || value.Type().Elem() != s.typ {
		panic("rs: a collection of " + s.typ.String() + " yielded a " + value.Type().String() +
			" where ReflectEntries and ReflectElements yield a pointer to the element")
	}
	return value.UnsafePointer()
}

func (s *shape) equalGoMap(a, b unsafe.Pointer) bool {
	left, right := reflect.NewAt(s.typ, a).Elem(), reflect.NewAt(s.typ, b).Elem()
	if left.Len() != right.Len() {
		return false
	}
	if left.Len() == 0 {
		return true
	}
	if left.UnsafePointer() == right.UnsafePointer() && s.key.isReflexive() && s.elem.isReflexive() {
		return true
	}
	leftValue, rightValue := reflect.New(s.elem.typ), reflect.New(s.elem.typ)
	entries := left.MapRange()
	for entries.Next() {
		other := right.MapIndex(entries.Key())
		if !other.IsValid() {
			return false
		}
		leftValue.Elem().SetIterValue(entries)
		rightValue.Elem().Set(other)
		if !s.elem.equalAt(leftValue.UnsafePointer(), rightValue.UnsafePointer()) {
			return false
		}
	}
	return true
}

// reflectEntry is one entry of a ReflectMap: the key as the collection
// handed it over, and the address of the value.
type reflectEntry struct {
	key   any
	value unsafe.Pointer
}

func (s *shape) reflectEntries(collection ReflectMap) []reflectEntry {
	entries := make([]reflectEntry, 0, collection.ReflectLen())
	collection.ReflectEntries(func(key, value any) bool {
		entries = append(entries, reflectEntry{key: key, value: s.elem.pointeeAddress(value)})
		return true
	})
	return entries
}

func (s *shape) reflectElements(collection ReflectSeq) []unsafe.Pointer {
	elements := make([]unsafe.Pointer, 0, collection.ReflectLen())
	collection.ReflectElements(func(element any) bool {
		elements = append(elements, s.elem.pointeeAddress(element))
		return true
	})
	return elements
}

func (s *shape) equalReflectMap(a, b unsafe.Pointer) bool {
	left := reflect.NewAt(s.typ, a).Interface().(ReflectMap)
	right := reflect.NewAt(s.typ, b).Interface().(ReflectMap)
	if left.ReflectLen() != right.ReflectLen() {
		return false
	}
	leftEntries, rightEntries := s.reflectEntries(left), s.reflectEntries(right)
	if len(leftEntries) != len(rightEntries) {
		return false
	}
	if left.ReflectSorted() && right.ReflectSorted() {
		// BTreeMap: `self.iter().zip(other).all(|(a, b)| a == b)`.
		for i, entry := range leftEntries {
			if !s.key.equalBoxed(entry.key, rightEntries[i].key) || !s.elem.equalAt(entry.value, rightEntries[i].value) {
				return false
			}
		}
		return true
	}
	// HashMap, IndexMap: every key of one is in the other with an equal
	// value, whatever the order.
	if s.key.goComparable() {
		index := make(map[any]unsafe.Pointer, len(rightEntries))
		for _, entry := range rightEntries {
			index[entry.key] = entry.value
		}
		for _, entry := range leftEntries {
			other, found := index[entry.key]
			if !found || !s.elem.equalAt(entry.value, other) {
				return false
			}
		}
		return true
	}
	for _, entry := range leftEntries {
		found := false
		for _, other := range rightEntries {
			if s.key.equalBoxed(entry.key, other.key) {
				found = s.elem.equalAt(entry.value, other.value)
				break
			}
		}
		if !found {
			return false
		}
	}
	return true
}

func (s *shape) equalReflectSeq(a, b unsafe.Pointer) bool {
	left := reflect.NewAt(s.typ, a).Interface().(ReflectSeq)
	right := reflect.NewAt(s.typ, b).Interface().(ReflectSeq)
	if left.ReflectLen() != right.ReflectLen() {
		return false
	}
	leftElements, rightElements := s.reflectElements(left), s.reflectElements(right)
	if len(leftElements) != len(rightElements) {
		return false
	}
	if !left.ReflectIsSet() || (left.ReflectSorted() && right.ReflectSorted()) {
		// VecDeque, BTreeSet: element by element.
		for i, element := range leftElements {
			if !s.elem.equalAt(element, rightElements[i]) {
				return false
			}
		}
		return true
	}
	// HashSet: every element of one is in the other.
	if s.elem.goComparable() {
		index := make(map[any]struct{}, len(rightElements))
		for _, element := range rightElements {
			index[reflect.NewAt(s.elem.typ, element).Elem().Interface()] = struct{}{}
		}
		for _, element := range leftElements {
			if _, found := index[reflect.NewAt(s.elem.typ, element).Elem().Interface()]; !found {
				return false
			}
		}
		return true
	}
	for _, element := range leftElements {
		found := false
		for _, other := range rightElements {
			if s.elem.equalAt(element, other) {
				found = true
				break
			}
		}
		if !found {
			return false
		}
	}
	return true
}

// goComparable reports whether Go's own == on values of the type agrees with
// Eq, so that a Go map can index them: integers, bools, strings, and arrays
// and structs of those, with no hand-written Equal.
func (s *shape) goComparable() bool {
	if s.equal.ok {
		return false
	}
	switch s.kind {
	case kindBool, kindInt, kindUint, kindString:
		return true
	case kindArray:
		return s.elem.goComparable()
	case kindStruct:
		for _, field := range s.fields {
			if !field.shape.goComparable() {
				return false
			}
		}
		// A blank field is not in fields, and Go's == skips it too.
		return true
	}
	return false
}

// isReflexive reports whether x == x holds for every value of the type, which
// is what lets two pointers to one value be taken as equal without looking.
func (s *shape) isReflexive() bool {
	switch s.reflexive.Load() {
	case 1:
		return true
	case 2:
		return false
	}
	reflexive := s.checkReflexive(map[*shape]bool{})
	if reflexive {
		s.reflexive.Store(1)
	} else {
		s.reflexive.Store(2)
	}
	return reflexive
}

func (s *shape) checkReflexive(seen map[*shape]bool) bool {
	if seen[s] {
		// Already being checked further up: it adds nothing new.
		return true
	}
	seen[s] = true
	if s.equal.ok {
		return false
	}
	switch s.kind {
	case kindFloat, kindComplex:
		return false
	case kindInterface:
		// Only an enum whose variants are all known can be checked.
		info := TypeInfoOf(s.typ)
		if info == nil || !info.IsEnum || len(info.Variants) == 0 {
			return false
		}
		for _, variant := range info.Variants {
			if variant.GoType == nil || !shapeOf(variant.GoType).checkReflexive(seen) {
				return false
			}
		}
		return true
	case kindPointer, kindSlice, kindArray, kindOption, kindReflectSeq, kindArc:
		return s.elem.checkReflexive(seen)
	case kindGoMap, kindReflectMap:
		return s.key.checkReflexive(seen) && s.elem.checkReflexive(seen)
	case kindStruct, kindResult:
		for _, field := range s.fields {
			if !field.shape.checkReflexive(seen) {
				return false
			}
		}
		return true
	}
	return true
}
