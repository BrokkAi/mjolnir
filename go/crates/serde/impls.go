// Port of serde_core 1.0.229 src/ser/impls.rs and src/de/impls.rs: the
// Serialize and Deserialize impls of the standard library's types, for the
// Go types that stand for them.

package serde

import (
	"encoding/json"
	"math"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

var (
	rsPkgPath           = reflect.TypeFor[rs.Unit]().PkgPath()
	stringType          = reflect.TypeFor[string]()
	int32Type           = reflect.TypeFor[int32]()
	intType             = reflect.TypeFor[int]()
	unitType            = reflect.TypeFor[rs.Unit]()
	durationType        = reflect.TypeFor[time.Duration]()
	timeType            = reflect.TypeFor[time.Time]()
	reflectMapType      = reflect.TypeFor[rs.ReflectMap]()
	reflectSeqType      = reflect.TypeFor[rs.ReflectSeq]()
	reflectArcType      = reflect.TypeFor[rs.ReflectArc]()
	jsonMarshalerType   = reflect.TypeFor[json.Marshaler]()
	jsonUnmarshalerType = reflect.TypeFor[json.Unmarshaler]()
)

func isRsGeneric(typ reflect.Type, name string) bool {
	return typ.Kind() == reflect.Struct && typ.PkgPath() == rsPkgPath && strings.HasPrefix(typ.Name(), name+"[")
}

// isOptionType reports whether typ is an rs.Option[T].
func isOptionType(typ reflect.Type) bool { return isRsGeneric(typ, "Option") }

// tupleArity returns the number of elements of an rs.Tuple2..5, or zero.
func tupleArity(typ reflect.Type) int {
	if typ.Kind() != reflect.Struct || typ.PkgPath() != rsPkgPath || !strings.HasPrefix(typ.Name(), "Tuple") {
		return 0
	}
	name := typ.Name()
	if len(name) < 7 || name[6] != '[' || name[5] < '2' || name[5] > '9' {
		return 0
	}
	return int(name[5] - '0')
}

func implementsOnPointer(typ reflect.Type, iface reflect.Type) bool {
	return typ.Kind() != reflect.Interface && typ.Kind() != reflect.Pointer && reflect.PointerTo(typ).Implements(iface)
}

// String renders the Rust type, for use as a cache key.
func (t *rustType) String() string {
	if t == nil {
		return "_"
	}
	if len(t.args) == 0 {
		return t.name
	}
	parts := make([]string, len(t.args))
	for index, arg := range t.args {
		parts[index] = arg.String()
	}
	return t.name + "<" + strings.Join(parts, ",") + ">"
}

// rustTypeMatters reports whether the Rust type a field declared changes how
// the Go type typ is serialized: whether somewhere inside there is a `char`
// held as an int32, an `isize` held as an int, or a path held as a string.
func rustTypeMatters(typ reflect.Type, rust *rustType) bool {
	if rust == nil {
		return false
	}
	rust = rust.peel()
	switch {
	case typ == stringType:
		return rust.name == "PathBuf" || rust.name == "Path"
	case typ == int32Type:
		return rust.name == "char"
	case typ == intType:
		return rust.name == "isize"
	}
	switch typ.Kind() {
	case reflect.Pointer:
		return rustTypeMatters(typ.Elem(), rust)
	case reflect.Slice, reflect.Array:
		if typ.PkgPath() != "" {
			return false
		}
		return rustTypeMatters(typ.Elem(), rust.arg(0))
	case reflect.Struct:
		switch {
		case isOptionType(typ):
			return rust.name == "Option" && rustTypeMatters(typ.Field(0).Type, rust.arg(0))
		case tupleArity(typ) > 0:
			if rust.name != "tuple" || len(rust.args) != typ.NumField() {
				return false
			}
			for index := range typ.NumField() {
				if rustTypeMatters(typ.Field(index).Type, rust.args[index]) {
					return true
				}
			}
		case implementsOnPointer(typ, reflectMapType):
			if len(rust.args) != 2 {
				return false
			}
			m := reflect.New(typ).Interface().(rs.ReflectMap)
			return rustTypeMatters(m.ReflectKeyType(), rust.args[0]) || rustTypeMatters(m.ReflectValueType(), rust.args[1])
		case implementsOnPointer(typ, reflectSeqType):
			if len(rust.args) != 1 {
				return false
			}
			return rustTypeMatters(reflect.New(typ).Interface().(rs.ReflectSeq).ReflectElemType(), rust.args[0])
		}
	}
	return false
}

// buildCodec fills in the codec of c.typ: the impls the type wrote by hand,
// and for whichever direction it did not write, the impl serde derives or
// the standard library's.
func buildCodec(c *codec) {
	var built codec
	built.typ, built.rust = c.typ, c.rust
	handWritten(&built)
	switch {
	case built.ser == nil && built.de == nil:
		buildStandard(&built)
	case built.ser == nil || built.de == nil:
		// One direction is written by hand. The other is the standard one,
		// and a type that has none (a foreign type with only a Serialize)
		// fails when that direction is first used, not here.
		var standard codec
		standard.typ, standard.rust = c.typ, c.rust
		failure := catchBuild(func() { buildStandard(&standard) })
		if built.ser == nil {
			built.ser, built.serVariant = standard.ser, standard.serVariant
			if failure != nil {
				built.ser = func(unsafe.Pointer, Serializer) (any, error) { panic(failure) }
			}
		}
		if built.de == nil {
			built.de = standard.de
			if failure != nil {
				built.de = func(unsafe.Pointer, Deserializer) error { panic(failure) }
			}
		}
	}
	c.ser, c.de, c.serVariant = built.ser, built.de, built.serVariant
	if c.serVariant == nil && c.typ.Kind() == reflect.Interface {
		// A hand-written Serialize of an enum takes the enum value.
		enumType, ser := c.typ, c.ser
		c.serVariant = func(variant reflect.Type, p unsafe.Pointer, serializer Serializer) (any, error) {
			holder := reflect.New(enumType)
			holder.Elem().Set(reflect.NewAt(variant, p))
			return ser(holder.UnsafePointer(), serializer)
		}
	}
}

// catchBuild runs build and returns what it panicked with, or nil.
func catchBuild(build func()) (failure any) {
	defer func() {
		if failure = recover(); failure != nil {
			// Forget the codecs the failed build left half made.
			for key, half := range building {
				if half.ser == nil || half.de == nil {
					delete(building, key)
				}
			}
		}
	}()
	build()
	return nil
}

func buildStandard(c *codec) {
	typ := c.typ
	rust := c.rust.peel()
	switch {
	case typ == durationType:
		c.ser, c.de = serDuration, deDuration
		return
	case typ == timeType:
		c.ser, c.de = serSystemTime, deSystemTime
		return
	case typ == unitType:
		c.ser = func(_ unsafe.Pointer, serializer Serializer) (any, error) { return serializer.SerializeUnit() }
		c.de = func(_ unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeUnit(unitVisitor{})
			return err
		}
		return
	}
	// The types of go/rs come before the registry, which knows a generic
	// type by its name alone.
	switch {
	case isOptionType(typ):
		buildOption(c, rust)
		return
	case tupleArity(typ) > 0:
		buildTuple(c, rust)
		return
	case implementsOnPointer(typ, reflectArcType):
		buildArc(c)
		return
	}
	if info := rs.TypeInfoOf(typ); info != nil && typ.Kind() != reflect.Pointer {
		if info.Enum != nil && !info.IsEnum {
			panic(rs.Panicf("serde: " + typ.String() + " is the variant " + info.RustName + " of " + info.Enum.RustName + "; serialize a pointer to it, which is a value of the enum"))
		}
		buildDerived(c, info)
		return
	}
	switch typ.Kind() {
	case reflect.Struct:
		switch {
		case implementsOnPointer(typ, reflectMapType):
			buildMap(c, rust)
		case implementsOnPointer(typ, reflectSeqType):
			buildSeqCollection(c, rust)
		case isJSONType(typ):
			buildJSON(c)
		default:
			// An unregistered struct: derive with the Go names.
			buildDerived(c, nil)
		}
	case reflect.Bool:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeBool(*(*bool)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeBool(boolVisitor{out: (*bool)(p)})
			return err
		}
	case reflect.Int8:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeI8(*(*int8)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeI8(intVisitor[int8]{out: (*int8)(p)})
			return err
		}
	case reflect.Int16:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeI16(*(*int16)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeI16(intVisitor[int16]{out: (*int16)(p)})
			return err
		}
	case reflect.Int32:
		if typ == int32Type && rust != nil && rust.name == "char" {
			c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
				return serializer.SerializeChar(*(*rune)(p))
			}
			c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
				_, err := deserializer.DeserializeChar(charVisitor{out: (*rune)(p)})
				return err
			}
			return
		}
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeI32(*(*int32)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeI32(intVisitor[int32]{out: (*int32)(p)})
			return err
		}
	case reflect.Int64:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeI64(*(*int64)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeI64(intVisitor[int64]{out: (*int64)(p)})
			return err
		}
	case reflect.Int:
		// A Go int is a Rust usize, unless the field says it is an isize.
		if typ == intType && rust != nil && rust.name == "isize" {
			c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
				return serializer.SerializeI64(int64(*(*int)(p)))
			}
			c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
				_, err := deserializer.DeserializeI64(intVisitor[isizeInt]{out: (*isizeInt)(p)})
				return err
			}
			return
		}
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			value := *(*int)(p)
			if value < 0 {
				panic(rs.Panicf("serde: a negative usize (" + strconv.Itoa(value) + "); a Go int stands for a usize unless its field is tagged `rust=isize`"))
			}
			if value == rs.UsizeMax {
				// usize::MAX, which the port holds as the largest int, is
				// written as Rust writes it (translation-rules.md, rule 3).
				return serializer.SerializeU64(rs.UsizeMaxU64)
			}
			return serializer.SerializeU64(uint64(value))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeU64(intVisitor[usizeInt]{out: (*usizeInt)(p)})
			return err
		}
	case reflect.Uint8:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeU8(*(*uint8)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeU8(intVisitor[uint8]{out: (*uint8)(p)})
			return err
		}
	case reflect.Uint16:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeU16(*(*uint16)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeU16(intVisitor[uint16]{out: (*uint16)(p)})
			return err
		}
	case reflect.Uint32:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeU32(*(*uint32)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeU32(intVisitor[uint32]{out: (*uint32)(p)})
			return err
		}
	case reflect.Uint64:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeU64(*(*uint64)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeU64(intVisitor[uint64]{out: (*uint64)(p)})
			return err
		}
	case reflect.Uint:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeU64(uint64(*(*uint)(p)))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeU64(intVisitor[usizeUint]{out: (*usizeUint)(p)})
			return err
		}
	case reflect.Float32:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeF32(*(*float32)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeF32(f32Visitor{out: (*float32)(p)})
			return err
		}
	case reflect.Float64:
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeF64(*(*float64)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeF64(f64Visitor{out: (*float64)(p)})
			return err
		}
	case reflect.String:
		if typ == stringType && rust != nil && (rust.name == "PathBuf" || rust.name == "Path") {
			c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
				path := *(*string)(p)
				if !utf8.ValidString(path) {
					return nil, SerErrorCustom("path contains invalid UTF-8 characters")
				}
				return serializer.SerializeStr(path)
			}
			c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
				_, err := deserializer.DeserializeString(pathBufVisitor{out: (*string)(p)})
				return err
			}
			return
		}
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return serializer.SerializeStr(*(*string)(p))
		}
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			_, err := deserializer.DeserializeString(stringVisitor{out: (*string)(p)})
			return err
		}
	case reflect.Slice:
		buildSlice(c, rust)
	case reflect.Array:
		buildArray(c, rust)
	case reflect.Pointer:
		buildPointer(c, rust)
	case reflect.Interface:
		if isJSONType(typ) {
			buildJSON(c)
			return
		}
		panic(rs.Panicf("serde: the interface " + typ.String() + " is not a registered enum and has no Serialize or Deserialize registered (rs.RegisterEnum, serde.RegisterSerialize, serde.RegisterDeserialize)"))
	default:
		if isJSONType(typ) {
			buildJSON(c)
			return
		}
		panic(rs.Panicf("serde: cannot serialize or deserialize the Go type " + typ.String()))
	}
}

// boolVisitor is `BoolVisitor`.
type boolVisitor struct{ out *bool }

func (boolVisitor) Expecting() string { return "a boolean" }
func (v boolVisitor) VisitBool(value bool) (any, error) {
	*v.out = value
	return nil, nil
}

// isizeInt, usizeInt and usizeUint are a Go int or uint read as the Rust
// type their name says.
type (
	isizeInt  int
	usizeInt  int
	usizeUint uint
)

type integer interface {
	int8 | int16 | int32 | int64 | isizeInt | uint8 | uint16 | uint32 | uint64 | usizeInt | usizeUint
}

// intVisitor is the `PrimitiveVisitor` of an integer type: it takes any
// integer that fits.
type intVisitor[T integer] struct{ out *T }

// intLimits returns the Rust name of T and its range as Go holds it.
func intLimits[T integer]() (name string, min int64, max uint64) {
	var zero T
	switch any(zero).(type) {
	case int8:
		return "i8", math.MinInt8, math.MaxInt8
	case int16:
		return "i16", math.MinInt16, math.MaxInt16
	case int32:
		return "i32", math.MinInt32, math.MaxInt32
	case int64:
		return "i64", math.MinInt64, math.MaxInt64
	case isizeInt:
		return "isize", math.MinInt, math.MaxInt
	case uint8:
		return "u8", 0, math.MaxUint8
	case uint16:
		return "u16", 0, math.MaxUint16
	case uint32:
		return "u32", 0, math.MaxUint32
	case uint64:
		return "u64", 0, math.MaxUint64
	case usizeInt:
		// A usize that a Go int cannot hold is out of range here.
		return "usize", 0, math.MaxInt
	case usizeUint:
		return "usize", 0, math.MaxUint
	}
	panic(rs.Unreachable("serde: integer type"))
}

func (v intVisitor[T]) Expecting() string {
	name, _, _ := intLimits[T]()
	return name
}

func (v intVisitor[T]) VisitI64(value int64) (any, error) {
	_, min, max := intLimits[T]()
	if value < min || (value > 0 && uint64(value) > max) {
		return nil, DeErrorInvalidValue(&DeUnexpectedSigned{F0: value}, v)
	}
	*v.out = T(value)
	return nil, nil
}

func (v intVisitor[T]) VisitU64(value uint64) (any, error) {
	_, _, max := intLimits[T]()
	if out, usize := any(v.out).(*usizeInt); usize && value == rs.UsizeMaxU64 {
		// usize::MAX is the largest int in the port (translation-rules.md,
		// rule 3). The usizes between that and usize::MAX cannot be held.
		*out = usizeInt(rs.UsizeMax)
		return nil, nil
	}
	if value > max {
		return nil, DeErrorInvalidValue(&DeUnexpectedUnsigned{F0: value}, v)
	}
	*v.out = T(value)
	return nil, nil
}

// f32Visitor and f64Visitor are the `PrimitiveVisitor` of f32 and f64: they
// take any number.
type f32Visitor struct{ out *float32 }

func (f32Visitor) Expecting() string { return "f32" }
func (v f32Visitor) VisitF32(value float32) (any, error) {
	*v.out = value
	return nil, nil
}
func (v f32Visitor) VisitF64(value float64) (any, error) {
	*v.out = float32(value)
	return nil, nil
}
func (v f32Visitor) VisitI64(value int64) (any, error) {
	*v.out = float32(value)
	return nil, nil
}
func (v f32Visitor) VisitU64(value uint64) (any, error) {
	*v.out = float32(value)
	return nil, nil
}

type f64Visitor struct{ out *float64 }

func (f64Visitor) Expecting() string { return "f64" }
func (v f64Visitor) VisitF64(value float64) (any, error) {
	*v.out = value
	return nil, nil
}
func (v f64Visitor) VisitI64(value int64) (any, error) {
	*v.out = float64(value)
	return nil, nil
}
func (v f64Visitor) VisitU64(value uint64) (any, error) {
	*v.out = float64(value)
	return nil, nil
}

// charVisitor is `CharVisitor`.
type charVisitor struct{ out *rune }

func (charVisitor) Expecting() string { return "a character" }
func (v charVisitor) VisitChar(value rune) (any, error) {
	*v.out = value
	return nil, nil
}
func (v charVisitor) VisitStr(value string) (any, error) {
	r, size := utf8.DecodeRuneInString(value)
	if size == 0 || size != len(value) {
		return nil, DeErrorInvalidValue(&DeUnexpectedStr{F0: value}, v)
	}
	*v.out = r
	return nil, nil
}

// stringVisitor is `StringVisitor`.
type stringVisitor struct{ out *string }

func (stringVisitor) Expecting() string { return "a string" }
func (v stringVisitor) VisitStr(value string) (any, error) {
	*v.out = value
	return nil, nil
}
func (v stringVisitor) VisitBytes(value []byte) (any, error) {
	if !utf8.Valid(value) {
		return nil, DeErrorInvalidValue(&DeUnexpectedBytes{F0: value}, v)
	}
	*v.out = string(value)
	return nil, nil
}

// pathBufVisitor is `PathBufVisitor`.
type pathBufVisitor struct{ out *string }

func (pathBufVisitor) Expecting() string { return "path string" }
func (v pathBufVisitor) VisitStr(value string) (any, error) {
	*v.out = value
	return nil, nil
}
func (v pathBufVisitor) VisitBytes(value []byte) (any, error) {
	if !utf8.Valid(value) {
		return nil, DeErrorInvalidValue(&DeUnexpectedBytes{F0: value}, v)
	}
	*v.out = string(value)
	return nil, nil
}

// buildOption is `impl Serialize for Option<T>` and `impl Deserialize for
// Option<T>`.
func buildOption(c *codec, rust *rustType) {
	typ := c.typ
	valueField, someField := typ.Field(0), typ.Field(1)
	if valueField.Name != "value" || someField.Name != "some" {
		panic(rs.Panicf("serde: rs.Option is not laid out as go/crates/serde expects"))
	}
	var elemRust *rustType
	if rust != nil && rust.name == "Option" {
		elemRust = rust.arg(0)
	}
	elem := childCodec(valueField.Type, elemRust)
	valueOffset, someOffset := valueField.Offset, someField.Offset
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		if !*(*bool)(unsafe.Add(p, someOffset)) {
			return serializer.SerializeNone()
		}
		return serializer.SerializeSome(&serializeRef{codec: elem, pointer: unsafe.Add(p, valueOffset)})
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		_, err := deserializer.DeserializeOption(&optionVisitor{
			elem:   elem,
			typ:    typ,
			option: p,
			value:  unsafe.Add(p, valueOffset),
			some:   (*bool)(unsafe.Add(p, someOffset)),
		})
		return err
	}
}

// optionVisitor is `OptionVisitor<T>`.
type optionVisitor struct {
	elem   *codec
	typ    reflect.Type
	option unsafe.Pointer
	value  unsafe.Pointer
	some   *bool
}

func (v *optionVisitor) Expecting() string { return "option" }

func (v *optionVisitor) none() (any, error) {
	// The value may hold what a failed attempt left there.
	reflect.NewAt(v.typ, v.option).Elem().SetZero()
	return nil, nil
}

func (v *optionVisitor) VisitUnit() (any, error) { return v.none() }
func (v *optionVisitor) VisitNone() (any, error) { return v.none() }

func (v *optionVisitor) VisitSome(deserializer Deserializer) (any, error) {
	if err := v.elem.de(v.value, deserializer); err != nil {
		return nil, err
	}
	*v.some = true
	return nil, nil
}

// privateVisitUntaggedOption is `__private_visit_untagged_option`: a
// flattened Option is None when its content does not deserialize.
func (v *optionVisitor) privateVisitUntaggedOption(deserializer Deserializer) (any, error) {
	if err := v.elem.de(v.value, deserializer); err != nil {
		return v.none()
	}
	*v.some = true
	return nil, nil
}

// sliceHeader is the layout of a Go slice.
type sliceHeader struct {
	data unsafe.Pointer
	len  int
	cap  int
}

// buildSlice is `impl Serialize for Vec<T>` (and `[T]`) and `impl
// Deserialize for Vec<T>`. A nil slice is an empty Vec.
func buildSlice(c *codec, rust *rustType) {
	typ := c.typ
	elemType := typ.Elem()
	elem := childCodec(elemType, rust.arg(0))
	size := elemType.Size()
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		header := (*sliceHeader)(p)
		seq, err := serializer.SerializeSeq(rs.Some(header.len))
		if err != nil {
			return nil, err
		}
		ref := &serializeRef{codec: elem}
		for index := range header.len {
			ref.pointer = unsafe.Add(header.data, uintptr(index)*size)
			if err := seq.SerializeElement(ref); err != nil {
				return nil, err
			}
		}
		return seq.End()
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		_, err := deserializer.DeserializeSeq(&sliceVisitor{typ: typ, elem: elem, out: p})
		return err
	}
}

// sliceVisitor is the `SeqVisitor` of `Vec<T>`.
type sliceVisitor struct {
	typ  reflect.Type
	elem *codec
	out  unsafe.Pointer
}

func (v *sliceVisitor) Expecting() string { return "a sequence" }

func (v *sliceVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	elemType := v.typ.Elem()
	capacity := sizeHintCautious(seq.SizeHint(), elemType.Size())
	values := reflect.New(v.typ).Elem()
	values.Set(reflect.MakeSlice(v.typ, 0, capacity))
	seed := &placeSeed{codec: v.elem}
	for {
		// Read the element into a fresh slot at the end, and keep it only
		// when there was one.
		length := values.Len()
		if length == values.Cap() {
			values.Grow(1)
		}
		values.SetLen(length + 1)
		seed.pointer = values.Index(length).Addr().UnsafePointer()
		_, ok, err := seq.NextElementSeed(seed)
		if err != nil {
			return nil, err
		}
		if !ok {
			values.SetLen(length)
			break
		}
	}
	reflect.NewAt(v.typ, v.out).Elem().Set(values)
	return nil, nil
}

// buildArray is `impl Serialize for [T; N]` and `impl Deserialize for [T;
// N]`.
func buildArray(c *codec, rust *rustType) {
	typ := c.typ
	length := typ.Len()
	elemType := typ.Elem()
	elem := childCodec(elemType, rust.arg(0))
	size := elemType.Size()
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		seq, err := serializer.SerializeTuple(length)
		if err != nil {
			return nil, err
		}
		ref := &serializeRef{codec: elem}
		for index := range length {
			ref.pointer = unsafe.Add(p, uintptr(index)*size)
			if err := seq.SerializeElement(ref); err != nil {
				return nil, err
			}
		}
		return seq.End()
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		_, err := deserializer.DeserializeTuple(length, &arrayVisitor{length: length, elem: elem, size: size, out: p})
		return err
	}
}

// arrayVisitor is `ArrayVisitor<[T; N]>`.
type arrayVisitor struct {
	length int
	elem   *codec
	size   uintptr
	out    unsafe.Pointer
}

func (v *arrayVisitor) Expecting() string {
	if v.length == 0 {
		return "an empty array"
	}
	return "an array of length " + strconv.Itoa(v.length)
}

func (v *arrayVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	seed := &placeSeed{codec: v.elem}
	for index := range v.length {
		seed.pointer = unsafe.Add(v.out, uintptr(index)*v.size)
		_, ok, err := seq.NextElementSeed(seed)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, DeErrorInvalidLength(index, v)
		}
	}
	return nil, nil
}

// buildTuple is the Serialize and Deserialize of a Rust tuple, held as an
// rs.Tuple2..5.
func buildTuple(c *codec, rust *rustType) {
	typ := c.typ
	count := typ.NumField()
	elems := make([]*codec, count)
	offsets := make([]uintptr, count)
	for index := range count {
		field := typ.Field(index)
		var elemRust *rustType
		if rust != nil && rust.name == "tuple" {
			elemRust = rust.arg(index)
		}
		elems[index] = childCodec(field.Type, elemRust)
		offsets[index] = field.Offset
	}
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		tuple, err := serializer.SerializeTuple(count)
		if err != nil {
			return nil, err
		}
		ref := &serializeRef{}
		for index := range count {
			ref.codec, ref.pointer = elems[index], unsafe.Add(p, offsets[index])
			if err := tuple.SerializeElement(ref); err != nil {
				return nil, err
			}
		}
		return tuple.End()
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		_, err := deserializer.DeserializeTuple(count, &tupleVisitor{elems: elems, offsets: offsets, out: p})
		return err
	}
}

// tupleVisitor is `TupleVisitor`.
type tupleVisitor struct {
	elems   []*codec
	offsets []uintptr
	out     unsafe.Pointer
}

func (v *tupleVisitor) Expecting() string { return "a tuple of size " + strconv.Itoa(len(v.elems)) }

func (v *tupleVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	seed := &placeSeed{}
	for index := range v.elems {
		seed.codec, seed.pointer = v.elems[index], unsafe.Add(v.out, v.offsets[index])
		_, ok, err := seq.NextElementSeed(seed)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, DeErrorInvalidLength(index, v)
		}
	}
	return nil, nil
}

// buildPointer is the Serialize and Deserialize of `Box<T>` (and of the
// `Arc<T>` and `Rc<T>` that the port holds as plain pointers): those of T.
// The pointer is never nil.
func buildPointer(c *codec, rust *rustType) {
	typ := c.typ
	elemType := typ.Elem()
	if variant := variantPointerInfo(typ); variant != nil {
		panic(rs.Panicf("serde: a field of type " + typ.String() + " holds one variant of " + variant.Enum.RustName + "; declare it with the enum's type"))
	}
	elem := childCodec(elemType, rust)
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		pointee := *(*unsafe.Pointer)(p)
		if pointee == nil {
			panic(rs.Panicf("serde: cannot serialize a nil " + typ.String() + "; nothing in Rust is nil"))
		}
		return elem.ser(pointee, serializer)
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		box := reflect.New(elemType)
		if err := elem.de(box.UnsafePointer(), deserializer); err != nil {
			return err
		}
		*(*unsafe.Pointer)(p) = box.UnsafePointer()
		return nil
	}
}

func variantPointerInfo(typ reflect.Type) *rs.TypeInfo {
	info := rs.TypeInfoOf(typ)
	if info != nil && info.Enum != nil && !info.IsEnum && info.Enum.GoType != nil && info.Enum.GoType.Kind() == reflect.Interface {
		return info
	}
	return nil
}

// buildArc is the Serialize and Deserialize of an `Arc<T>` held as an
// rs.Arc[T] (serde's `rc` feature): those of T.
func buildArc(c *codec) {
	typ := c.typ
	elemType := reflect.New(typ).Interface().(rs.ReflectArc).ReflectArcType()
	elem := childCodec(elemType, nil)
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		pointee := reflect.ValueOf(reflect.NewAt(typ, p).Interface().(rs.ReflectArc).ReflectArcGet()).UnsafePointer()
		return elem.ser(pointee, serializer)
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		value := reflect.New(elemType)
		if err := elem.de(value.UnsafePointer(), deserializer); err != nil {
			return err
		}
		reflect.NewAt(typ, p).Interface().(rs.ReflectArc).ReflectArcSet(value.Elem().Interface())
		return nil
	}
}

// buildMap is `impl Serialize` and `impl Deserialize` for `BTreeMap<K, V>`,
// `HashMap<K, V>` and the other maps, which describe themselves through
// rs.ReflectMap. The zero value is an empty map.
func buildMap(c *codec, rust *rustType) {
	typ := c.typ
	probe := reflect.New(typ).Interface().(rs.ReflectMap)
	keyType, valueType := probe.ReflectKeyType(), probe.ReflectValueType()
	var keyRust, valueRust *rustType
	if rust != nil && len(rust.args) == 2 {
		keyRust, valueRust = rust.args[0], rust.args[1]
	}
	key, value := childCodec(keyType, keyRust), childCodec(valueType, valueRust)
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		source := reflect.NewAt(typ, p).Interface().(rs.ReflectMap)
		m, err := serializer.SerializeMap(rs.Some(source.ReflectLen()))
		if err != nil {
			return nil, err
		}
		keyRef, valueRef := &serializeRef{codec: key}, &serializeRef{codec: value}
		keyHolder := reflect.New(keyType)
		emit := func(entryKey any, entryValue any) bool {
			keyHolder.Elem().Set(reflect.ValueOf(entryKey))
			keyRef.pointer = keyHolder.UnsafePointer()
			valueRef.pointer = reflect.ValueOf(entryValue).UnsafePointer()
			err = m.SerializeEntry(keyRef, valueRef)
			return err == nil
		}
		if source.ReflectSorted() || !orderedKind(keyType.Kind()) {
			source.ReflectEntries(emit)
		} else {
			// A HashMap iterates in an order Rust leaves unspecified; the
			// port writes it in key order so that output is reproducible.
			type entry struct{ key, value any }
			entries := make([]entry, 0, source.ReflectLen())
			source.ReflectEntries(func(entryKey any, entryValue any) bool {
				entries = append(entries, entry{entryKey, entryValue})
				return true
			})
			slices.SortFunc(entries, func(a, b entry) int { return compareOrderedKeys(a.key, b.key) })
			for _, found := range entries {
				if !emit(found.key, found.value) {
					break
				}
			}
		}
		if err != nil {
			return nil, err
		}
		return m.End()
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		_, err := deserializer.DeserializeMap(&mapVisitor{typ: typ, key: key, value: value, keyType: keyType, valueType: valueType, out: p})
		return err
	}
}

// mapVisitor is the `MapVisitor` of a map.
type mapVisitor struct {
	typ       reflect.Type
	key       *codec
	value     *codec
	keyType   reflect.Type
	valueType reflect.Type
	out       unsafe.Pointer
}

func (v *mapVisitor) Expecting() string { return "a map" }

func (v *mapVisitor) VisitMap(m DeMapAccess) (any, error) {
	values := reflect.New(v.typ)
	target := values.Interface().(rs.ReflectMap)
	keyHolder, valueHolder := reflect.New(v.keyType), reflect.New(v.valueType)
	keySeed := &placeSeed{codec: v.key, pointer: keyHolder.UnsafePointer()}
	valueSeed := &placeSeed{codec: v.value, pointer: valueHolder.UnsafePointer()}
	for {
		keyHolder.Elem().SetZero()
		valueHolder.Elem().SetZero()
		_, _, ok, err := m.NextEntrySeed(keySeed, valueSeed)
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		target.ReflectInsert(keyHolder.Elem().Interface(), valueHolder.Elem().Interface())
	}
	reflect.NewAt(v.typ, v.out).Elem().Set(values.Elem())
	return nil, nil
}

// buildSeqCollection is `impl Serialize` and `impl Deserialize` for
// `BTreeSet<T>`, `HashSet<T>`, `VecDeque<T>` and the other sequences, which
// describe themselves through rs.ReflectSeq.
func buildSeqCollection(c *codec, rust *rustType) {
	typ := c.typ
	elemType := reflect.New(typ).Interface().(rs.ReflectSeq).ReflectElemType()
	elem := childCodec(elemType, rust.arg(0))
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		source := reflect.NewAt(typ, p).Interface().(rs.ReflectSeq)
		seq, err := serializer.SerializeSeq(rs.Some(source.ReflectLen()))
		if err != nil {
			return nil, err
		}
		ref := &serializeRef{codec: elem}
		source.ReflectElements(func(element any) bool {
			ref.pointer = reflect.ValueOf(element).UnsafePointer()
			err = seq.SerializeElement(ref)
			return err == nil
		})
		if err != nil {
			return nil, err
		}
		return seq.End()
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		_, err := deserializer.DeserializeSeq(&seqCollectionVisitor{typ: typ, elem: elem, elemType: elemType, out: p})
		return err
	}
}

// seqCollectionVisitor is the `SeqVisitor` of a set or a deque.
type seqCollectionVisitor struct {
	typ      reflect.Type
	elem     *codec
	elemType reflect.Type
	out      unsafe.Pointer
}

func (v *seqCollectionVisitor) Expecting() string { return "a sequence" }

func (v *seqCollectionVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	values := reflect.New(v.typ)
	target := values.Interface().(rs.ReflectSeq)
	holder := reflect.New(v.elemType)
	seed := &placeSeed{codec: v.elem, pointer: holder.UnsafePointer()}
	for {
		holder.Elem().SetZero()
		_, ok, err := seq.NextElementSeed(seed)
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		target.ReflectPush(holder.Elem().Interface())
	}
	reflect.NewAt(v.typ, v.out).Elem().Set(values.Elem())
	return nil, nil
}

const nanosPerSec = 1_000_000_000

// serDuration is `impl Serialize for Duration`.
func serDuration(p unsafe.Pointer, serializer Serializer) (any, error) {
	duration := *(*time.Duration)(p)
	if duration < 0 {
		panic(rs.Panicf("serde: cannot serialize the negative duration " + duration.String() + "; a Rust Duration is never negative"))
	}
	return serializeSecsNanos(serializer, "Duration", "secs", "nanos", uint64(duration/nanosPerSec), uint32(duration%nanosPerSec))
}

func serializeSecsNanos(serializer Serializer, name string, secsField string, nanosField string, secs uint64, nanos uint32) (any, error) {
	state, err := serializer.SerializeStruct(name, 2)
	if err != nil {
		return nil, err
	}
	if err := state.SerializeField(secsField, &serializeRef{codec: codecOf(reflect.TypeFor[uint64]()), pointer: unsafe.Pointer(&secs)}); err != nil {
		return nil, err
	}
	if err := state.SerializeField(nanosField, &serializeRef{codec: codecOf(reflect.TypeFor[uint32]()), pointer: unsafe.Pointer(&nanos)}); err != nil {
		return nil, err
	}
	return state.End()
}

// deDuration is `impl Deserialize for Duration`. A duration longer than a Go
// time.Duration can hold (292 years) becomes the longest one.
func deDuration(p unsafe.Pointer, deserializer Deserializer) error {
	visitor := &durationVisitor{
		expecting:     "struct Duration",
		fields:        durationFields,
		fieldExpected: "`secs` or `nanos`",
		overflow:      "overflow deserializing Duration",
	}
	if _, err := deserializer.DeserializeStruct("Duration", durationFields, visitor); err != nil {
		return err
	}
	*(*time.Duration)(p) = visitor.duration()
	return nil
}

var (
	durationFields   = []string{"secs", "nanos"}
	systemTimeFields = []string{"secs_since_epoch", "nanos_since_epoch"}
)

// durationVisitor is the `DurationVisitor` of Duration and of SystemTime.
type durationVisitor struct {
	expecting     string
	fields        []string
	fieldExpected string
	overflow      string
	secs          uint64
	nanos         uint32
}

func (v *durationVisitor) Expecting() string { return v.expecting }

// duration returns the Duration::new(secs, nanos) the visitor read, as far
// as a time.Duration can hold it.
func (v *durationVisitor) duration() time.Duration {
	secs := v.secs + uint64(v.nanos/nanosPerSec)
	nanos := uint64(v.nanos % nanosPerSec)
	if secs > uint64(math.MaxInt64/nanosPerSec) || secs*nanosPerSec > uint64(math.MaxInt64)-nanos {
		return time.Duration(math.MaxInt64)
	}
	return time.Duration(secs*nanosPerSec + nanos)
}

func (v *durationVisitor) checkOverflow() error {
	if v.secs > math.MaxUint64-uint64(v.nanos/nanosPerSec) {
		return DeErrorCustom(v.overflow)
	}
	return nil
}

func (v *durationVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	_, ok, err := seq.NextElementSeed(SeedFor(&v.secs))
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, DeErrorInvalidLength(0, v)
	}
	_, ok, err = seq.NextElementSeed(SeedFor(&v.nanos))
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, DeErrorInvalidLength(1, v)
	}
	return nil, v.checkOverflow()
}

func (v *durationVisitor) VisitMap(m DeMapAccess) (any, error) {
	hasSecs, hasNanos := false, false
	for {
		key, ok, err := m.NextKeySeed(&durationFieldVisitor{parent: v})
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		if key.(int) == 0 {
			if hasSecs {
				return nil, DeErrorDuplicateField(v.fields[0])
			}
			if _, err := m.NextValueSeed(SeedFor(&v.secs)); err != nil {
				return nil, err
			}
			hasSecs = true
		} else {
			if hasNanos {
				return nil, DeErrorDuplicateField(v.fields[1])
			}
			if _, err := m.NextValueSeed(SeedFor(&v.nanos)); err != nil {
				return nil, err
			}
			hasNanos = true
		}
	}
	if !hasSecs {
		return nil, DeErrorMissingField(v.fields[0])
	}
	if !hasNanos {
		return nil, DeErrorMissingField(v.fields[1])
	}
	return nil, v.checkOverflow()
}

// durationFieldVisitor is the `FieldVisitor` of Duration and SystemTime. Its
// value is the index of the field.
type durationFieldVisitor struct{ parent *durationVisitor }

func (f *durationFieldVisitor) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIdentifier(f)
}

func (f *durationFieldVisitor) Expecting() string { return f.parent.fieldExpected }

func (f *durationFieldVisitor) VisitStr(value string) (any, error) {
	switch value {
	case f.parent.fields[0]:
		return 0, nil
	case f.parent.fields[1]:
		return 1, nil
	}
	return nil, DeErrorUnknownField(value, f.parent.fields)
}

func (f *durationFieldVisitor) VisitBytes(value []byte) (any, error) {
	switch string(value) {
	case f.parent.fields[0]:
		return 0, nil
	case f.parent.fields[1]:
		return 1, nil
	}
	return nil, DeErrorUnknownField(strings.ToValidUTF8(string(value), "�"), f.parent.fields)
}

// serSystemTime is `impl Serialize for SystemTime`.
func serSystemTime(p unsafe.Pointer, serializer Serializer) (any, error) {
	instant := *(*time.Time)(p)
	secs, nanos := instant.Unix(), instant.Nanosecond()
	if secs < 0 {
		return nil, SerErrorCustom("SystemTime must be later than UNIX_EPOCH")
	}
	return serializeSecsNanos(serializer, "SystemTime", "secs_since_epoch", "nanos_since_epoch", uint64(secs), uint32(nanos))
}

// deSystemTime is `impl Deserialize for SystemTime`.
func deSystemTime(p unsafe.Pointer, deserializer Deserializer) error {
	visitor := &durationVisitor{
		expecting:     "struct SystemTime",
		fields:        systemTimeFields,
		fieldExpected: "`secs_since_epoch` or `nanos_since_epoch`",
		overflow:      "overflow deserializing SystemTime epoch offset",
	}
	if _, err := deserializer.DeserializeStruct("SystemTime", systemTimeFields, visitor); err != nil {
		return err
	}
	secs := visitor.secs + uint64(visitor.nanos/nanosPerSec)
	// UNIX_EPOCH.checked_add fails past the last instant a timespec holds:
	// i64::MAX seconds.
	if secs > math.MaxInt64 {
		return DeErrorCustom("overflow deserializing SystemTime")
	}
	*(*time.Time)(p) = time.Unix(int64(secs), int64(visitor.nanos%nanosPerSec))
	return nil
}

// isJSONType reports whether typ knows only JSON: it implements
// json.Marshaler or json.Unmarshaler, as the types of acp-go do.
func isJSONType(typ reflect.Type) bool {
	return typ.Implements(jsonMarshalerType) || typ.Implements(jsonUnmarshalerType) ||
		(typ.Kind() != reflect.Interface && typ.Kind() != reflect.Pointer &&
			(reflect.PointerTo(typ).Implements(jsonMarshalerType) || reflect.PointerTo(typ).Implements(jsonUnmarshalerType)))
}

// buildJSON serializes and deserializes a type that knows only JSON through
// the JSON bridge.
func buildJSON(c *codec) {
	typ := c.typ
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		if jsonBridge.SerializeJSON == nil {
			panic(rs.Panicf("serde: " + typ.String() + " serializes through encoding/json; import go/crates/serdejson, which installs the bridge"))
		}
		text, err := json.Marshal(reflect.NewAt(typ, p).Interface())
		if err != nil {
			return nil, SerErrorCustom(err)
		}
		return jsonBridge.SerializeJSON(text, serializer)
	}
	c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
		if jsonBridge.DeserializeJSON == nil {
			panic(rs.Panicf("serde: " + typ.String() + " deserializes through encoding/json; import go/crates/serdejson, which installs the bridge"))
		}
		text, err := jsonBridge.DeserializeJSON(deserializer)
		if err != nil {
			return err
		}
		if err := json.Unmarshal(text, reflect.NewAt(typ, p).Interface()); err != nil {
			return DeErrorCustom(err)
		}
		return nil
	}
}

func orderedKind(kind reflect.Kind) bool {
	switch kind {
	case reflect.String, reflect.Bool,
		reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		return true
	}
	return false
}

// compareOrderedKeys orders two map keys of one ordered kind.
func compareOrderedKeys(a any, b any) int {
	left, right := reflect.ValueOf(a), reflect.ValueOf(b)
	switch left.Kind() {
	case reflect.String:
		return strings.Compare(left.String(), right.String())
	case reflect.Bool:
		switch {
		case left.Bool() == right.Bool():
			return 0
		case !left.Bool():
			return -1
		}
		return 1
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		switch {
		case left.Int() < right.Int():
			return -1
		case left.Int() > right.Int():
			return 1
		}
		return 0
	}
	switch {
	case left.Uint() < right.Uint():
		return -1
	case left.Uint() > right.Uint():
		return 1
	}
	return 0
}
