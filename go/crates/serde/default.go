package serde

import (
	"reflect"
	"sync"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The Default of a type, which `#[serde(default)]` asks for.
//
// go/rs owns what `T::default()` is (rs.Default, rs.RegisterDefault), but it
// can only be asked through a type parameter, and the engine has a
// reflect.Type. Until go/rs can be asked by reflection, a ported type whose
// Default the engine needs says so once, next to its registration:
//
//	var _ = serde.RegisterDefaultOf[UiConfig]()
//
// which records rs.Default[UiConfig] for the engine. The types of the
// standard library need nothing: their Default is the Go zero value, a new
// allocation for a Box, or their elements' Default.

var defaultThunks sync.Map // reflect.Type -> func(dst unsafe.Pointer)

// RegisterDefaultOf records that the Default of T is rs.Default[T](), so
// that `#[serde(default)]` on a field of type T, or on T itself, can build
// one. It returns true so that it can initialise a package variable.
func RegisterDefaultOf[T any]() bool {
	defaultThunks.Store(reflect.TypeFor[T](), func(dst unsafe.Pointer) { *(*T)(dst) = rs.Default[T]() })
	return true
}

// defaultSetter returns the function that stores `Default::default()` of
// typ in a value of that type. where names the attribute that needs it, for
// the panic of a type whose Default the engine cannot know.
func defaultSetter(typ reflect.Type, where string) func(dst unsafe.Pointer) {
	if thunk, ok := defaultThunks.Load(typ); ok {
		return thunk.(func(dst unsafe.Pointer))
	}
	if implementsOnPointer(typ, reflect.TypeFor[rs.ReflectDefault]()) {
		return func(dst unsafe.Pointer) {
			reflect.NewAt(typ, dst).Elem().SetZero()
			reflect.NewAt(typ, dst).Interface().(rs.ReflectDefault).ReflectSetDefault()
		}
	}
	zero := func(dst unsafe.Pointer) { reflect.NewAt(typ, dst).Elem().SetZero() }
	switch {
	case isOptionType(typ), typ == durationType, typ == unitType,
		implementsOnPointer(typ, reflectMapType), implementsOnPointer(typ, reflectSeqType):
		return zero
	case tupleArity(typ) > 0:
		return structuralDefault(typ, where)
	}
	if info := rs.TypeInfoOf(typ); info != nil && typ.Kind() != reflect.Pointer {
		panic(rs.Panicf("serde: " + where + " needs the Default of " + typ.String() + ", which go/crates/serde cannot ask go/rs for by reflection; add `var _ = serde.RegisterDefaultOf[" + typ.Name() + "]()` next to the type"))
	}
	switch typ.Kind() {
	case reflect.Bool, reflect.String, reflect.Slice,
		reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64,
		reflect.Float32, reflect.Float64:
		return zero
	case reflect.Pointer:
		// A Box<T> or an Arc<T>: a new allocation holding T's Default.
		elemType := typ.Elem()
		elem := defaultSetter(elemType, where)
		return func(dst unsafe.Pointer) {
			box := reflect.New(elemType)
			elem(box.UnsafePointer())
			*(*unsafe.Pointer)(dst) = box.UnsafePointer()
		}
	case reflect.Array:
		elemType := typ.Elem()
		elem := defaultSetter(elemType, where)
		length, size := typ.Len(), elemType.Size()
		return func(dst unsafe.Pointer) {
			for index := range length {
				elem(unsafe.Add(dst, uintptr(index)*size))
			}
		}
	case reflect.Struct:
		if typ.PkgPath() == "" || typ.NumField() == 0 {
			return structuralDefault(typ, where)
		}
		// A struct the port did not register (a type of the Go standard
		// library, a private helper): its zero value.
		return zero
	}
	panic(rs.Panicf("serde: " + where + " needs the Default of " + typ.String() + ", which has none; register it with serde.RegisterDefaultOf"))
}

// structuralDefault is a derived Default: every field's Default.
func structuralDefault(typ reflect.Type, where string) func(dst unsafe.Pointer) {
	type part struct {
		offset uintptr
		set    func(dst unsafe.Pointer)
	}
	parts := make([]part, typ.NumField())
	for index := range typ.NumField() {
		field := typ.Field(index)
		parts[index] = part{offset: field.Offset, set: defaultSetter(field.Type, where)}
	}
	return func(dst unsafe.Pointer) {
		for _, p := range parts {
			p.set(unsafe.Add(dst, p.offset))
		}
	}
}
