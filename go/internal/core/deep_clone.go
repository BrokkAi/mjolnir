// The Go equivalent of Rust's derived Clone for core's data types.
//
// Rust stages a state change on `snapshot.clone()` and adopts the copy only
// once it is durable, so a failed budget check or write leaves the live
// snapshot untouched. A Go struct copy shares its maps, slices and pointed-to
// values, and ApplyRelayEvent mutates those in place, so staging (and any
// other `.clone()` that is then changed) needs a deep copy.

package core

import "reflect"

// Clone returns a deep copy of the snapshot, as Rust's derived Clone does:
// maps, slices, pointed-to values and interface payloads are copied, so
// applying an event to the copy never changes s.
func (s *RelaySnapshot) Clone() RelaySnapshot {
	return DeepClone(*s)
}

// DeepClone copies value recursively through every exported path, as a
// derived Rust Clone does. Unexported struct fields are copied as values
// (their own maps and pointers stay shared); none of core's data types reach
// mutable data through them.
func DeepClone[T any](value T) T {
	original := reflect.ValueOf(&value).Elem()
	clone := reflect.New(original.Type()).Elem()
	deepCloneInto(clone, original)
	return clone.Interface().(T)
}

func deepCloneInto(dst, src reflect.Value) {
	switch src.Kind() {
	case reflect.Pointer:
		if src.IsNil() {
			return
		}
		copied := reflect.New(src.Type().Elem())
		deepCloneInto(copied.Elem(), src.Elem())
		dst.Set(copied)
	case reflect.Interface:
		if src.IsNil() {
			return
		}
		element := src.Elem()
		copied := reflect.New(element.Type()).Elem()
		deepCloneInto(copied, element)
		dst.Set(copied)
	case reflect.Map:
		if src.IsNil() {
			return
		}
		copied := reflect.MakeMapWithSize(src.Type(), src.Len())
		entries := src.MapRange()
		for entries.Next() {
			key := reflect.New(src.Type().Key()).Elem()
			deepCloneInto(key, entries.Key())
			value := reflect.New(src.Type().Elem()).Elem()
			deepCloneInto(value, entries.Value())
			copied.SetMapIndex(key, value)
		}
		dst.Set(copied)
	case reflect.Slice:
		if src.IsNil() {
			return
		}
		copied := reflect.MakeSlice(src.Type(), src.Len(), src.Len())
		for index := 0; index < src.Len(); index++ {
			deepCloneInto(copied.Index(index), src.Index(index))
		}
		dst.Set(copied)
	case reflect.Array:
		for index := 0; index < src.Len(); index++ {
			deepCloneInto(dst.Index(index), src.Index(index))
		}
	case reflect.Struct:
		dst.Set(src)
		for index := 0; index < src.NumField(); index++ {
			if field := dst.Field(index); field.CanSet() {
				field.SetZero()
				deepCloneInto(field, src.Field(index))
			}
		}
	default:
		dst.Set(src)
	}
}
