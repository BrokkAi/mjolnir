package serde

import (
	"reflect"
	"strings"
	"sync"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The functions serde attributes name: `default = "path"`,
// `skip_serializing_if = "path"`, `serialize_with = "path"`,
// `deserialize_with = "path"` and `with = "module"`.
//
// Rust resolves the path where the attribute is written. Go cannot, so the
// path is a key: the string in the struct tag (or in the registration's
// attributes) is looked up exactly as it is written among the functions
// registered here. A key must therefore name one function. `is_false` is
// four different functions in mj-core alone, so the declarations write the
// function's absolute path, crate first, in both places:
//
//	Enabled bool `rs:"enabled" serde:"skip_serializing_if=mj_core::config::is_false"`
//
//	var _ = serde.RegisterSkipSerializingIf("mj_core::config::is_false", Config_is_false)
//
// Registering two different functions under one key panics, as does an
// attribute whose key is not registered. The paths of the standard library
// that serde attributes use are built in, in the spellings doc.go lists.

type fnKind uint8

const (
	fnDefault fnKind = iota
	fnSkipSerializingIf
	fnSerializeWith
	fnDeserializeWith
)

func (kind fnKind) attribute() string {
	switch kind {
	case fnDefault:
		return "default"
	case fnSkipSerializingIf:
		return "skip_serializing_if"
	case fnSerializeWith:
		return "serialize_with"
	}
	return "deserialize_with"
}

// fnEntry is one registered function, adapted to the address of the field
// it works on.
type fnEntry struct {
	kind fnKind
	// typ is the type of the field the function is for. nil is a function
	// that takes the field as an `any` and so suits every type.
	typ reflect.Type
	// identity tells one registered function from another.
	identity uintptr
	// bind gives a function registered with an `any` parameter the type of
	// the field it is used on.
	bind   func(field reflect.Type) fnEntry
	setDef func(p unsafe.Pointer)
	skip   func(p unsafe.Pointer) bool
	ser    func(p unsafe.Pointer, serializer Serializer) (any, error)
	de     func(p unsafe.Pointer, deserializer Deserializer) error
}

var (
	fnMu  sync.RWMutex
	fnMap = map[string][]fnEntry{}
)

func registerFn(path string, entry fnEntry, function any) bool {
	path = strings.TrimSpace(path)
	entry.identity = reflect.ValueOf(function).Pointer()
	fnMu.Lock()
	defer fnMu.Unlock()
	for _, existing := range fnMap[path] {
		if existing.kind != entry.kind || existing.typ != entry.typ {
			continue
		}
		if existing.identity == entry.identity {
			return true
		}
		panic(rs.Panicf("serde: two different functions are registered as `" + path + "` for #[serde(" + entry.kind.attribute() + ")]; a path must name one function, so register each under its absolute Rust path"))
	}
	fnMap[path] = append(fnMap[path], entry)
	return true
}

// RegisterDefault registers the function a `#[serde(default = "path")]`
// names. It returns true so that it can initialise a package variable.
func RegisterDefault[T any](path string, function func() T) bool {
	return registerFn(path, fnEntry{
		kind:   fnDefault,
		typ:    reflect.TypeFor[T](),
		setDef: func(p unsafe.Pointer) { *(*T)(p) = function() },
	}, function)
}

// RegisterSkipSerializingIf registers the function a
// `#[serde(skip_serializing_if = "path")]` names. Rust's function takes
// `&T`.
func RegisterSkipSerializingIf[T any](path string, function func(value *T) bool) bool {
	return registerFn(path, fnEntry{
		kind: fnSkipSerializingIf,
		typ:  reflect.TypeFor[T](),
		skip: func(p unsafe.Pointer) bool { return function((*T)(p)) },
	}, function)
}

// RegisterSerializeWith registers the function a `#[serde(serialize_with =
// "path")]` names, or the `serialize` of a `#[serde(with = "module")]`
// under `module::serialize`.
func RegisterSerializeWith[T any](path string, function func(value *T, serializer Serializer) (any, error)) bool {
	return registerFn(path, fnEntry{
		kind: fnSerializeWith,
		typ:  reflect.TypeFor[T](),
		ser:  func(p unsafe.Pointer, serializer Serializer) (any, error) { return function((*T)(p), serializer) },
	}, function)
}

// RegisterDeserializeWith registers the function a
// `#[serde(deserialize_with = "path")]` names, or the `deserialize` of a
// `#[serde(with = "module")]` under `module::deserialize`.
func RegisterDeserializeWith[T any](path string, function func(deserializer Deserializer) (T, error)) bool {
	return registerFn(path, fnEntry{
		kind: fnDeserializeWith,
		typ:  reflect.TypeFor[T](),
		de: func(p unsafe.Pointer, deserializer Deserializer) error {
			value, err := function(deserializer)
			if err != nil {
				return err
			}
			*(*T)(p) = value
			return nil
		},
	}, function)
}

var (
	serializerType   = reflect.TypeFor[Serializer]()
	deserializerType = reflect.TypeFor[Deserializer]()
	errorType        = reflect.TypeFor[error]()
	anyType          = reflect.TypeFor[any]()
	boolType         = reflect.TypeFor[bool]()
)

// RegisterFn registers a function that a serde attribute names, working
// out which attribute it is for from its signature:
//
//	func() T                                       default
//	func(*T) bool, func(T) bool                    skip_serializing_if
//	func(*T, Serializer) (any, error)              serialize_with
//	func(T, Serializer) (any, error)               serialize_with
//	func(Deserializer) (T, error)                  deserialize_with
//
// A skip_serializing_if or serialize_with function whose first parameter is
// `any` is given a pointer to the field and suits a field of any type,
// which is how a generic Rust function is registered. The typed Register
// functions do the same without reflection when the function is called.
func RegisterFn(path string, function any) bool {
	value := reflect.ValueOf(function)
	typ := value.Type()
	if typ.Kind() != reflect.Func {
		panic(rs.Panicf("serde.RegisterFn(" + path + "): not a function"))
	}
	in, out := typ.NumIn(), typ.NumOut()
	switch {
	case in == 0 && out == 1:
		result := typ.Out(0)
		return registerFn(path, fnEntry{kind: fnDefault, typ: result, setDef: func(p unsafe.Pointer) {
			reflect.NewAt(result, p).Elem().Set(value.Call(nil)[0])
		}}, function)
	case in == 1 && out == 1 && typ.Out(0) == boolType:
		adapt := func(argument func(p unsafe.Pointer) reflect.Value) func(p unsafe.Pointer) bool {
			return func(p unsafe.Pointer) bool {
				return value.Call([]reflect.Value{argument(p)})[0].Bool()
			}
		}
		field, argument := fnFieldType(typ.In(0))
		entry := fnEntry{kind: fnSkipSerializingIf, typ: field}
		if field == nil {
			entry.bind = func(fieldType reflect.Type) fnEntry {
				return fnEntry{kind: fnSkipSerializingIf, typ: fieldType, skip: adapt(func(p unsafe.Pointer) reflect.Value {
					return reflect.NewAt(fieldType, p)
				})}
			}
		} else {
			entry.skip = adapt(argument)
		}
		return registerFn(path, entry, function)
	case in == 2 && out == 2 && typ.In(1) == serializerType && typ.Out(1) == errorType:
		adapt := func(argument func(p unsafe.Pointer) reflect.Value) func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return func(p unsafe.Pointer, serializer Serializer) (any, error) {
				results := value.Call([]reflect.Value{argument(p), reflect.ValueOf(&serializer).Elem()})
				err, _ := results[1].Interface().(error)
				return results[0].Interface(), err
			}
		}
		field, argument := fnFieldType(typ.In(0))
		entry := fnEntry{kind: fnSerializeWith, typ: field}
		if field == nil {
			entry.bind = func(fieldType reflect.Type) fnEntry {
				return fnEntry{kind: fnSerializeWith, typ: fieldType, ser: adapt(func(p unsafe.Pointer) reflect.Value {
					return reflect.NewAt(fieldType, p)
				})}
			}
		} else {
			entry.ser = adapt(argument)
		}
		return registerFn(path, entry, function)
	case in == 1 && out == 2 && typ.In(0) == deserializerType && typ.Out(1) == errorType:
		result := typ.Out(0)
		return registerFn(path, fnEntry{kind: fnDeserializeWith, typ: result, de: func(p unsafe.Pointer, deserializer Deserializer) error {
			results := value.Call([]reflect.Value{reflect.ValueOf(&deserializer).Elem()})
			if err, _ := results[1].Interface().(error); err != nil {
				return err
			}
			reflect.NewAt(result, p).Elem().Set(results[0])
			return nil
		}}, function)
	}
	panic(rs.Panicf("serde.RegisterFn(" + path + "): " + typ.String() + " is not the signature of a default, skip_serializing_if, serialize_with or deserialize_with function"))
}

// fnFieldType returns the field type a function's first parameter stands
// for, and how to make the argument from the field's address: the pointer
// for a `*T` or an `any` parameter, the value for a `T` one.
func fnFieldType(parameter reflect.Type) (reflect.Type, func(p unsafe.Pointer) reflect.Value) {
	switch {
	case parameter == anyType:
		// The field's type is not known here; lookupFn supplies it.
		return nil, nil
	case parameter.Kind() == reflect.Pointer:
		field := parameter.Elem()
		return field, func(p unsafe.Pointer) reflect.Value { return reflect.NewAt(field, p) }
	}
	return parameter, func(p unsafe.Pointer) reflect.Value { return reflect.NewAt(parameter, p).Elem() }
}

// lookupFn finds the function an attribute of the given kind names for a
// field of type typ. where describes the field for the panic of a path that
// is not registered.
func lookupFn(kind fnKind, path string, typ reflect.Type, where string) fnEntry {
	path = strings.TrimSpace(path)
	fnMu.RLock()
	entries := fnMap[path]
	fnMu.RUnlock()
	var generic *fnEntry
	var others []string
	for index := range entries {
		entry := &entries[index]
		if entry.kind != kind {
			continue
		}
		switch {
		case entry.typ == typ:
			return *entry
		case entry.typ == nil:
			generic = entry
		default:
			others = append(others, entry.typ.String())
		}
	}
	if generic != nil {
		return generic.bind(typ)
	}
	if kind == fnSkipSerializingIf {
		if skip := builtinSkip(path, typ); skip != nil {
			return fnEntry{kind: kind, typ: typ, skip: skip}
		}
	}
	if len(others) > 0 {
		panic(rs.Panicf("serde: " + where + ": #[serde(" + kind.attribute() + " = \"" + path + "\")] is registered for " + strings.Join(others, ", ") + ", not for the field's type " + typ.String()))
	}
	panic(rs.Panicf("serde: " + where + ": no function is registered for #[serde(" + kind.attribute() + " = \"" + path + "\")] (serde.RegisterFn)" + similarPaths(path)))
}

// similarPaths lists the registered paths that end like path, to help find
// a key that was written differently.
func similarPaths(path string) string {
	last := path
	if index := strings.LastIndex(path, "::"); index >= 0 {
		last = path[index+2:]
	}
	fnMu.RLock()
	defer fnMu.RUnlock()
	var found []string
	for registered := range fnMap {
		if registered == last || strings.HasSuffix(registered, "::"+last) {
			found = append(found, "`"+registered+"`")
		}
	}
	if len(found) == 0 {
		return ""
	}
	return "; registered with the same name: " + strings.Join(found, ", ")
}

// builtinSkip returns the standard library function that a
// skip_serializing_if path names, for a field of type typ, or nil.
func builtinSkip(path string, typ reflect.Type) func(p unsafe.Pointer) bool {
	segments := strings.Split(path, "::")
	for index := range segments {
		segments[index] = strings.TrimSpace(segments[index])
	}
	last := segments[len(segments)-1]
	owner := ""
	if len(segments) >= 2 {
		owner = segments[len(segments)-2]
	}
	switch {
	case owner == "Option" && (last == "is_none" || last == "is_some") && isOptionType(typ):
		offset := typ.Field(1).Offset
		if last == "is_none" {
			return func(p unsafe.Pointer) bool { return !*(*bool)(unsafe.Add(p, offset)) }
		}
		return func(p unsafe.Pointer) bool { return *(*bool)(unsafe.Add(p, offset)) }
	case owner == "Not" && last == "not" && typ.Kind() == reflect.Bool:
		// std::ops::Not::not
		return func(p unsafe.Pointer) bool { return !*(*bool)(p) }
	case last == "is_empty":
		switch {
		case typ.Kind() == reflect.String:
			return func(p unsafe.Pointer) bool { return len(*(*string)(p)) == 0 }
		case typ.Kind() == reflect.Slice:
			return func(p unsafe.Pointer) bool { return (*sliceHeader)(p).len == 0 }
		case implementsOnPointer(typ, reflectMapType):
			return func(p unsafe.Pointer) bool {
				return reflect.NewAt(typ, p).Interface().(rs.ReflectMap).ReflectLen() == 0
			}
		case implementsOnPointer(typ, reflectSeqType):
			return func(p unsafe.Pointer) bool {
				return reflect.NewAt(typ, p).Interface().(rs.ReflectSeq).ReflectLen() == 0
			}
		}
	}
	return nil
}
