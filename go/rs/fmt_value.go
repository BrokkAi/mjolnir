// Port of core 1.99.1 src/fmt/mod.rs, src/fmt/num.rs and src/fmt/float.rs:
// the `impl Display`, `impl Debug`, `impl LowerHex` ... blocks, which in Go
// become a choice by the value's type.

package rs

import (
	"fmt"
	"math"
	"reflect"
	"sort"
	"strings"
	"sync"
	"time"
	"unsafe"
)

// LowerHex is Rust's fmt::LowerHex: how a value prints with `{:x}`. The
// integers have it built in.
type LowerHex interface {
	FmtLowerHex(f *Formatter) error
}

// UpperHex is Rust's fmt::UpperHex: how a value prints with `{:X}`.
type UpperHex interface {
	FmtUpperHex(f *Formatter) error
}

// Octal is Rust's fmt::Octal: how a value prints with `{:o}`.
type Octal interface {
	FmtOctal(f *Formatter) error
}

// Binary is Rust's fmt::Binary: how a value prints with `{:b}`.
type Binary interface {
	FmtBinary(f *Formatter) error
}

// LowerExp is Rust's fmt::LowerExp: how a value prints with `{:e}`. The
// integers and the floats have it built in.
type LowerExp interface {
	FmtLowerExp(f *Formatter) error
}

// UpperExp is Rust's fmt::UpperExp: how a value prints with `{:E}`.
type UpperExp interface {
	FmtUpperExp(f *Formatter) error
}

// Pointer is Rust's fmt::Pointer: how a value prints with `{:p}`. Go
// pointers, slices and strings have it built in.
type Pointer interface {
	FmtPointer(f *Formatter) error
}

// DisplayFmt writes value to f as `{}` would with f's own flags: it is
// `fmt::Display::fmt(&value, f)`, the way a Display impl delegates to a field.
func DisplayFmt(value any, f *Formatter) error { return fmtDisplay(value, f) }

// DebugFmt writes value to f as `{:?}` would with f's own flags: it is
// `fmt::Debug::fmt(&value, f)`.
func DebugFmt(value any, f *Formatter) error { return fmtDebug(value, f) }

// LowerHexFmt is `fmt::LowerHex::fmt(&value, f)`.
func LowerHexFmt(value any, f *Formatter) error { return fmtRadixValue(value, 16, false, f) }

// UpperHexFmt is `fmt::UpperHex::fmt(&value, f)`.
func UpperHexFmt(value any, f *Formatter) error { return fmtRadixValue(value, 16, true, f) }

// OctalFmt is `fmt::Octal::fmt(&value, f)`.
func OctalFmt(value any, f *Formatter) error { return fmtRadixValue(value, 8, false, f) }

// BinaryFmt is `fmt::Binary::fmt(&value, f)`.
func BinaryFmt(value any, f *Formatter) error { return fmtRadixValue(value, 2, false, f) }

// LowerExpFmt is `fmt::LowerExp::fmt(&value, f)`.
func LowerExpFmt(value any, f *Formatter) error { return fmtExp(value, false, f) }

// UpperExpFmt is `fmt::UpperExp::fmt(&value, f)`.
func UpperExpFmt(value any, f *Formatter) error { return fmtExp(value, true, f) }

// PointerFmt is `fmt::Pointer::fmt(&value, f)`.
func PointerFmt(value any, f *Formatter) error { return fmtPointer(value, f) }

// fmtNilText is what a nil pointer or a nil interface prints as. Rust has no
// nil: reaching this is an error in the port (an Option should have been
// used), made visible in the output rather than turned into a panic inside a
// log line or a failed assertion's message.
const fmtNilText = "<nil>"

var (
	fmtDisplayType    = reflect.TypeFor[Display]()
	fmtDebugType      = reflect.TypeFor[Debug]()
	fmtErrorType      = reflect.TypeFor[error]()
	fmtStringerType   = reflect.TypeFor[fmt.Stringer]()
	fmtInt128Type     = reflect.TypeFor[fmtInt128]()
	fmtReflectMapType = reflect.TypeFor[ReflectMap]()
	fmtReflectSeqType = reflect.TypeFor[ReflectSeq]()
	fmtDurationType   = reflect.TypeFor[time.Duration]()
	fmtTimeType       = reflect.TypeFor[time.Time]()
	fmtCharType       = reflect.TypeFor[Char]()
)

// fmtMethods says which of the formatter's interfaces a type implements, on
// the type itself (direct) or only on a pointer to it (viaPointer), in which
// case a value is formatted through its address.
type fmtMethods struct {
	direct     bool
	viaPointer bool
}

func fmtMethodsOf(t reflect.Type, iface reflect.Type) fmtMethods {
	if t.Implements(iface) {
		return fmtMethods{direct: true}
	}
	if t.Kind() != reflect.Pointer && t.Kind() != reflect.Interface && reflect.PointerTo(t).Implements(iface) {
		return fmtMethods{viaPointer: true}
	}
	return fmtMethods{}
}

func (m fmtMethods) any() bool { return m.direct || m.viaPointer }

// fmtTypeInfo is what the formatter needs to know about a Go type, worked
// out once.
type fmtTypeInfo struct {
	display    fmtMethods
	debug      fmtMethods
	isError    fmtMethods
	stringer   fmtMethods
	int128     fmtMethods
	reflectMap fmtMethods
	reflectSeq fmtMethods
	// named is whether the type, with its pointers taken off, has a name
	// and so may be in the registry.
	named bool
	// fields describes a struct's fields.
	fields []fmtFieldInfo
	// tupleFields is whether the fields are named F0, F1, ..., as those of a
	// ported tuple struct are.
	tupleFields bool
}

type fmtFieldInfo struct {
	// rustName is the `rs` tag's name, or the Go name in snake_case.
	rustName string
	exported bool
}

var fmtTypeInfoCache sync.Map // reflect.Type -> *fmtTypeInfo

func fmtTypeInfoOf(t reflect.Type) *fmtTypeInfo {
	if cached, ok := fmtTypeInfoCache.Load(t); ok {
		return cached.(*fmtTypeInfo)
	}
	info := &fmtTypeInfo{
		display:    fmtMethodsOf(t, fmtDisplayType),
		debug:      fmtMethodsOf(t, fmtDebugType),
		isError:    fmtMethodsOf(t, fmtErrorType),
		stringer:   fmtMethodsOf(t, fmtStringerType),
		int128:     fmtMethodsOf(t, fmtInt128Type),
		reflectMap: fmtMethodsOf(t, fmtReflectMapType),
		reflectSeq: fmtMethodsOf(t, fmtReflectSeqType),
	}
	base := t
	for base.Kind() == reflect.Pointer {
		base = base.Elem()
	}
	info.named = base.Name() != ""
	if t.Kind() == reflect.Struct {
		info.fields = make([]fmtFieldInfo, t.NumField())
		info.tupleFields = t.NumField() > 0
		for i := range info.fields {
			field := t.Field(i)
			// The Rust name is the `rs` tag's (registry.go, FieldRustName);
			// a field without one gets its Go name back in snake_case.
			name := fmtSnakeCase(field.Name)
			if tag, ok := field.Tag.Lookup("rs"); ok {
				if tagName, _, _ := strings.Cut(tag, ","); tagName != "" {
					name = tagName
				}
			}
			info.fields[i] = fmtFieldInfo{rustName: name, exported: field.IsExported()}
			if field.Name != "F"+fmtItoa(i) {
				info.tupleFields = false
			}
		}
	}
	actual, _ := fmtTypeInfoCache.LoadOrStore(t, info)
	return actual.(*fmtTypeInfo)
}

func fmtItoa(i int) string {
	if i < 10 {
		return string(rune('0' + i))
	}
	return fmtItoa(i/10) + string(rune('0'+i%10))
}

// fmtSnakeCase turns a Go field name back into the Rust spelling it was
// derived from: SessionId is session_id, awaitsLeft is awaits_left. A run of
// capitals is one word (HTTPPort is http_port).
func fmtSnakeCase(name string) string {
	runes := []rune(name)
	out := make([]rune, 0, len(runes)+4)
	isUpper := func(c rune) bool { return c >= 'A' && c <= 'Z' }
	isLower := func(c rune) bool { return c >= 'a' && c <= 'z' }
	isDigit := func(c rune) bool { return c >= '0' && c <= '9' }
	for i, c := range runes {
		if isUpper(c) {
			if i > 0 {
				prev := runes[i-1]
				nextIsLower := i+1 < len(runes) && isLower(runes[i+1])
				if isLower(prev) || isDigit(prev) || (isUpper(prev) && nextIsLower) {
					out = append(out, '_')
				}
			}
			out = append(out, c-'A'+'a')
		} else {
			out = append(out, c)
		}
	}
	return string(out)
}

// fmtGoTypeName names a value's Go type for a message.
func fmtGoTypeName(v reflect.Value) string {
	if !v.IsValid() {
		return "nil"
	}
	return v.Type().String()
}

// fmtBaseTypeName is a type's own name, without its package, its type
// arguments and the pointers in front of it: what an unregistered type is
// called in Debug output.
func fmtBaseTypeName(t reflect.Type) string {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	name := t.Name()
	for i := 0; i < len(name); i++ {
		if name[i] == '[' {
			name = name[:i]
			break
		}
	}
	return name
}

// fmtAddressable returns v itself when its address can be taken, and
// otherwise a copy that is: methods with pointer receivers and unexported
// fields are reached through the address.
func fmtAddressable(v reflect.Value) reflect.Value {
	if v.CanAddr() {
		return v
	}
	copied := reflect.New(v.Type()).Elem()
	copied.Set(v)
	return copied
}

// fmtField returns field i of an addressable struct. A private Rust field is
// an unexported Go field, which reflection hands out read-only; going
// through its address gives a value whose methods can be called.
func fmtField(v reflect.Value, i int, exported bool) reflect.Value {
	field := v.Field(i)
	if exported {
		return field
	}
	return reflect.NewAt(field.Type(), unsafe.Pointer(field.UnsafeAddr())).Elem()
}

// fmtIsNil reports whether v is a nil pointer or a nil interface.
func fmtIsNil(v reflect.Value) bool {
	switch v.Kind() {
	case reflect.Pointer, reflect.Interface:
		return v.IsNil()
	}
	return false
}

func fmtAbs64(v int64) (abs uint64, isNonnegative bool) {
	if v < 0 {
		return uint64(-(v + 1)) + 1, false
	}
	return uint64(v), true
}

func fmtBoolText(v bool) string {
	if v {
		return "true"
	}
	return "false"
}

// fmtDisplay is `<T as Display>::fmt` for a Go value.
//
// A value prints with `{}` when it has a FmtDisplay method, is an error
// (its Error text) or a fmt.Stringer (its String text), or is a string, a
// boolean, a number or a pointer to one of these. Anything else has no
// Display in Rust either, and panics.
func fmtDisplay(value any, f *Formatter) error {
	switch v := value.(type) {
	case nil:
		return f.buf.writeStr(fmtNilText)
	case string:
		return f.Pad(v)
	case bool:
		return f.Pad(fmtBoolText(v))
	case int:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDisplayInt(abs, isNonnegative, f)
	case int8:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDisplayInt(abs, isNonnegative, f)
	case int16:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDisplayInt(abs, isNonnegative, f)
	case int32:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDisplayInt(abs, isNonnegative, f)
	case int64:
		abs, isNonnegative := fmtAbs64(v)
		return fmtDisplayInt(abs, isNonnegative, f)
	case uint:
		return fmtDisplayInt(uint64(v), true, f)
	case uint8:
		return fmtDisplayInt(uint64(v), true, f)
	case uint16:
		return fmtDisplayInt(uint64(v), true, f)
	case uint32:
		return fmtDisplayInt(uint64(v), true, f)
	case uint64:
		return fmtDisplayInt(v, true, f)
	case uintptr:
		return fmtDisplayInt(uint64(v), true, f)
	case float64:
		return fmtFloatToDecimalDisplay(f, fmtFloatOfF64(math.Float64bits(v)))
	case float32:
		return fmtFloatToDecimalDisplay(f, fmtFloatOfF32(math.Float32bits(v)))
	case fmtInt128:
		// u128 and i128: the formatter's own, before any method of theirs.
		if fmtIsNil(reflect.ValueOf(value)) {
			return f.buf.writeStr(fmtNilText)
		}
		hi, lo, signed := v.fmtInt128()
		return fmtDisplayInt128(hi, lo, signed, f)
	case Display:
		if fmtIsNil(reflect.ValueOf(value)) {
			return f.buf.writeStr(fmtNilText)
		}
		return v.FmtDisplay(f)
	}
	return fmtDisplayReflect(reflect.ValueOf(value), f)
}

func fmtDisplayReflect(v reflect.Value, f *Formatter) error {
	if !v.IsValid() || fmtIsNil(v) {
		return f.buf.writeStr(fmtNilText)
	}
	t := v.Type()
	kind := t.Kind()
	if kind == reflect.Interface {
		return fmtDisplayReflect(v.Elem(), f)
	}
	info := fmtTypeInfoOf(t)

	// u128 and i128: the formatter's own, before any method of theirs.
	if info.int128.any() {
		hi, lo, signed := fmtInt128Of(v, info)
		return fmtDisplayInt128(hi, lo, signed, f)
	}

	// impl Display
	if info.display.direct {
		return v.Interface().(Display).FmtDisplay(f)
	} else if info.display.viaPointer {
		return fmtAddressable(v).Addr().Interface().(Display).FmtDisplay(f)
	}

	if kind == reflect.Pointer {
		// impl<T: Display> Display for &T, and for Box<T>, Rc<T>, Arc<T>
		return fmtDisplayReflect(v.Elem(), f)
	}

	// Duration and SystemTime have no Display in Rust; their Go types have a
	// String method that prints something else.
	if t == fmtDurationType || t == fmtTimeType {
		panic(fmtNotImplemented(v, "Display", "{}"))
	}

	// An error prints its message and a Stringer its text, padded as a
	// string is.
	if info.isError.direct {
		return f.Pad(v.Interface().(error).Error())
	} else if info.isError.viaPointer {
		return f.Pad(fmtAddressable(v).Addr().Interface().(error).Error())
	}
	if info.stringer.direct {
		return f.Pad(v.Interface().(fmt.Stringer).String())
	} else if info.stringer.viaPointer {
		return f.Pad(fmtAddressable(v).Addr().Interface().(fmt.Stringer).String())
	}

	// A type the port registered stands for a Rust struct, enum or newtype,
	// which has Display only when the Rust code implements it, and then the
	// Go type has FmtDisplay.
	if info.named && TypeInfoOf(t) != nil {
		panic(fmtNotImplemented(v, "Display", "{}"))
	}

	switch kind {
	case reflect.String:
		return f.Pad(v.String())
	case reflect.Bool:
		return f.Pad(fmtBoolText(v.Bool()))
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		abs, isNonnegative := fmtAbs64(v.Int())
		return fmtDisplayInt(abs, isNonnegative, f)
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return fmtDisplayInt(v.Uint(), true, f)
	case reflect.Float64:
		return fmtFloatToDecimalDisplay(f, fmtFloatOfF64(math.Float64bits(v.Float())))
	case reflect.Float32:
		return fmtFloatToDecimalDisplay(f, fmtFloatOfF32(math.Float32bits(float32(v.Float()))))
	}
	panic(fmtNotImplemented(v, "Display", "{}"))
}

// fmtNotImplemented is the panic for a value that lacks the trait its
// placeholder asks for: a compile error in Rust.
func fmtNotImplemented(v reflect.Value, trait string, spec string) *PanicInfo {
	return fmtPanicInfo("`" + fmtGoTypeName(v) + "` doesn't implement `rs." + trait + "` (it cannot be formatted with `" + spec + "`)")
}

// fmtInt128Of reads a 128-bit integer through its fmtInt128 method.
func fmtInt128Of(v reflect.Value, info *fmtTypeInfo) (hi, lo uint64, signed bool) {
	if info.int128.direct {
		return v.Interface().(fmtInt128).fmtInt128()
	}
	return fmtAddressable(v).Addr().Interface().(fmtInt128).fmtInt128()
}

// fmtDebug is `<T as Debug>::fmt` for a Go value.
func fmtDebug(value any, f *Formatter) error {
	switch v := value.(type) {
	case nil:
		return f.buf.writeStr(fmtNilText)
	case string:
		return fmtDebugStr(v, f)
	case bool:
		return f.Pad(fmtBoolText(v))
	case int:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDebugInt(uint64(uint(v)), abs, isNonnegative, f)
	case int8:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDebugInt(uint64(uint8(v)), abs, isNonnegative, f)
	case int16:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDebugInt(uint64(uint16(v)), abs, isNonnegative, f)
	case int32:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtDebugInt(uint64(uint32(v)), abs, isNonnegative, f)
	case int64:
		abs, isNonnegative := fmtAbs64(v)
		return fmtDebugInt(uint64(v), abs, isNonnegative, f)
	case uint:
		return fmtDebugInt(uint64(v), uint64(v), true, f)
	case uint8:
		return fmtDebugInt(uint64(v), uint64(v), true, f)
	case uint16:
		return fmtDebugInt(uint64(v), uint64(v), true, f)
	case uint32:
		return fmtDebugInt(uint64(v), uint64(v), true, f)
	case uint64:
		return fmtDebugInt(v, v, true, f)
	case uintptr:
		return fmtDebugInt(uint64(v), uint64(v), true, f)
	case float64:
		return fmtFloatToGeneralDebug(f, fmtFloatOfF64(math.Float64bits(v)))
	case float32:
		return fmtFloatToGeneralDebug(f, fmtFloatOfF32(math.Float32bits(v)))
	case fmtInt128:
		// u128 and i128: the formatter's own, before any method of theirs.
		if fmtIsNil(reflect.ValueOf(value)) {
			return f.buf.writeStr(fmtNilText)
		}
		hi, lo, signed := v.fmtInt128()
		return fmtDebugInt128(hi, lo, signed, f)
	case Debug:
		if fmtIsNil(reflect.ValueOf(value)) {
			return f.buf.writeStr(fmtNilText)
		}
		return v.FmtDebug(f)
	}
	return fmtDebugReflect(reflect.ValueOf(value), f)
}

// fmtDebugReflect is Debug for a value reached by reflection: an argument
// that is not of a built-in type, a struct's field, a slice's element. The
// value must not have been obtained through an unexported field (fmtField
// sees to that).
func fmtDebugReflect(v reflect.Value, f *Formatter) error {
	if !v.IsValid() || fmtIsNil(v) {
		return f.buf.writeStr(fmtNilText)
	}
	t := v.Type()
	kind := t.Kind()
	if kind == reflect.Interface {
		return fmtDebugReflect(v.Elem(), f)
	}
	info := fmtTypeInfoOf(t)

	// u128 and i128: the formatter's own, before any method of theirs.
	if info.int128.any() {
		hi, lo, signed := fmtInt128Of(v, info)
		return fmtDebugInt128(hi, lo, signed, f)
	}

	// A hand-written impl Debug.
	if info.debug.direct {
		return v.Interface().(Debug).FmtDebug(f)
	} else if info.debug.viaPointer {
		return fmtAddressable(v).Addr().Interface().(Debug).FmtDebug(f)
	}

	if kind == reflect.Pointer {
		// impl<T: Debug> Debug for &T, and for Box<T>, Rc<T>, Arc<T>
		return fmtDebugReflect(v.Elem(), f)
	}

	switch t {
	case fmtDurationType:
		return fmtDebugDuration(time.Duration(v.Int()), f)
	case fmtTimeType:
		return fmtDebugSystemTime(v.Interface().(time.Time), f)
	}

	// The collections describe themselves.
	if info.reflectMap.direct {
		return fmtDebugReflectMap(v.Interface().(ReflectMap), f)
	} else if info.reflectMap.viaPointer {
		return fmtDebugReflectMap(fmtAddressable(v).Addr().Interface().(ReflectMap), f)
	}
	if info.reflectSeq.direct {
		return fmtDebugReflectSeq(v.Interface().(ReflectSeq), f)
	} else if info.reflectSeq.viaPointer {
		return fmtDebugReflectSeq(fmtAddressable(v).Addr().Interface().(ReflectSeq), f)
	}

	// A type the port registered: the Debug that Rust derives for it.
	if info.named {
		if registered := TypeInfoOf(t); registered != nil {
			return fmtDebugRegistered(v, info, registered, f)
		}
	}

	// An error that is not a ported type: see fmtDebugForeignError.
	if info.isError.direct {
		return fmtDebugForeignError(t, v.Interface().(error), f)
	} else if info.isError.viaPointer {
		return fmtDebugForeignError(t, fmtAddressable(v).Addr().Interface().(error), f)
	}

	return fmtDebugKind(v, info, f)
}

// fmtDebugForeignError is `{:?}` for a Go error that has no FmtDebug method
// and is not a registered type: an error of the Go standard library or of a
// Go module, which no Rust type corresponds to.
//
// Rust's error types mostly derive Debug (`ParseIntError { kind: InvalidDigit
// }`, `Utf8Error { valid_up_to: 0, error_len: Some(1) }`) or wrap a message
// (`Custom { kind: Other, error: "..." }`); none prints as its Display text
// in quotes alone. A Go error's fields cannot be walked safely (they are
// private to another package and may hold locks or cycles), so it prints as
// a tuple struct of its type's name around its message:
// `PathError("open x: no such file or directory")`. Packages that stand in
// for Rust libraries give their errors FmtDebug and never get here.
func fmtDebugForeignError(t reflect.Type, err error, f *Formatter) error {
	name := fmtBaseTypeName(t)
	if name == "" {
		name = "Error"
	}
	return f.DebugTuple(name).Field(err.Error()).Finish()
}

// fmtDebugRegistered is a derived Debug: the output of `#[derive(Debug)]` on
// the Rust type that the Go type registered as.
func fmtDebugRegistered(v reflect.Value, info *fmtTypeInfo, registered *TypeInfo, f *Formatter) error {
	kind := v.Kind()

	if registered.IsEnum {
		// A data-free enum: the value is one of the registered constants.
		// (An enum with data is an interface; its values are pointers to
		// variant structs, which are registered themselves.)
		for _, variant := range registered.Variants {
			if variant.Value != nil && fmtSameConstant(reflect.ValueOf(variant.Value), v) {
				return f.WriteStr(variant.RustName)
			}
		}
		// Not a declared variant, which Rust cannot hold: show the raw value.
		return f.DebugTuple(registered.RustName).
			fieldWith(func(f *Formatter) error { return fmtDebugKind(v, info, f) }).
			Finish()
	}

	if kind != reflect.Struct {
		switch registered.Shape {
		case ShapeUnit:
			return f.WriteStr(registered.RustName)
		default:
			// A newtype held as a defined type: `Name(inner)`.
			return f.DebugTuple(registered.RustName).
				fieldWith(func(f *Formatter) error { return fmtDebugKind(v, info, f) }).
				Finish()
		}
	}

	switch registered.Shape {
	case ShapeUnit:
		return f.WriteStr(registered.RustName)
	case ShapeStruct:
		return fmtDebugStructFields(v, info, registered.RustName, f)
	default:
		return fmtDebugTupleFields(v, info, registered.RustName, f)
	}
}

// fmtSameConstant reports whether an enum value equals a registered constant.
func fmtSameConstant(constant, v reflect.Value) bool {
	if constant.Kind() != v.Kind() {
		return false
	}
	switch v.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return constant.Int() == v.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return constant.Uint() == v.Uint()
	case reflect.String:
		return constant.String() == v.String()
	case reflect.Bool:
		return constant.Bool() == v.Bool()
	default:
		return reflect.DeepEqual(constant.Interface(), v.Interface())
	}
}

// fmtDebugStructFields prints `Name { field: value, .. }` from a struct's
// fields, as derive(Debug) does for a struct with named fields.
func fmtDebugStructFields(v reflect.Value, info *fmtTypeInfo, name string, f *Formatter) error {
	if len(info.fields) == 0 {
		return f.WriteStr(name)
	}
	v = fmtAddressable(v)
	builder := f.DebugStruct(name)
	for i := range info.fields {
		field := fmtField(v, i, info.fields[i].exported)
		builder.fieldWith(info.fields[i].rustName, func(f *Formatter) error {
			return fmtDebugReflect(field, f)
		})
	}
	return builder.Finish()
}

// fmtDebugTupleFields prints `Name(value, ..)` from a struct's fields, as
// derive(Debug) does for a tuple struct.
func fmtDebugTupleFields(v reflect.Value, info *fmtTypeInfo, name string, f *Formatter) error {
	if len(info.fields) == 0 {
		return f.WriteStr(name)
	}
	v = fmtAddressable(v)
	builder := f.DebugTuple(name)
	for i := range info.fields {
		field := fmtField(v, i, info.fields[i].exported)
		builder.fieldWith(func(f *Formatter) error {
			return fmtDebugReflect(field, f)
		})
	}
	return builder.Finish()
}

// fmtDebugKind is Debug for a value by what it is made of, whatever its
// type is called: the Rust type a Go built-in type stands for.
func fmtDebugKind(v reflect.Value, info *fmtTypeInfo, f *Formatter) error {
	switch v.Kind() {
	case reflect.String:
		return fmtDebugStr(v.String(), f)
	case reflect.Bool:
		return f.Pad(fmtBoolText(v.Bool()))
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		value := v.Int()
		abs, isNonnegative := fmtAbs64(value)
		return fmtDebugInt(fmtTruncate(uint64(value), v.Type().Bits()), abs, isNonnegative, f)
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return fmtDebugInt(v.Uint(), v.Uint(), true, f)
	case reflect.Float64:
		return fmtFloatToGeneralDebug(f, fmtFloatOfF64(math.Float64bits(v.Float())))
	case reflect.Float32:
		return fmtFloatToGeneralDebug(f, fmtFloatOfF32(math.Float32bits(float32(v.Float()))))
	case reflect.Slice:
		// impl<T: Debug> Debug for [T], which Vec<T> forwards to
		list := f.DebugList()
		for i := 0; i < v.Len(); i++ {
			element := v.Index(i)
			list.inner.entryWith(func(f *Formatter) error { return fmtDebugReflect(element, f) })
		}
		return list.Finish()
	case reflect.Array:
		// impl<T: Debug, const N: usize> Debug for [T; N]
		v = fmtAddressable(v)
		list := f.DebugList()
		for i := 0; i < v.Len(); i++ {
			element := v.Index(i)
			list.inner.entryWith(func(f *Formatter) error { return fmtDebugReflect(element, f) })
		}
		return list.Finish()
	case reflect.Map:
		// A Go map stands for a Rust map whose order the program may not
		// depend on; the entries are written in key order.
		m := f.DebugMap()
		for _, key := range fmtSortedMapKeys(v) {
			value := v.MapIndex(key)
			m.keyWith(func(f *Formatter) error { return fmtDebugReflect(key, f) })
			m.valueWith(func(f *Formatter) error { return fmtDebugReflect(value, f) })
		}
		return m.Finish()
	case reflect.Struct:
		// A struct the port did not register: its Go name and its Go field
		// names in snake_case, or a tuple struct when the fields are F0, F1, ...
		name := fmtBaseTypeName(v.Type())
		if name == "" {
			name = "struct"
		}
		if info.tupleFields {
			return fmtDebugTupleFields(v, info, name, f)
		}
		return fmtDebugStructFields(v, info, name, f)
	case reflect.Pointer:
		return fmtDebugReflect(v.Elem(), f)
	case reflect.Func, reflect.Chan, reflect.UnsafePointer:
		// impl Debug for fn(..) -> R and for *const T: the address.
		return fmtPointerFmtInner(v.Pointer(), f)
	}
	panic(fmtNotImplemented(v, "Debug", "{:?}"))
}

// fmtTruncate keeps the low bits of a sign-extended integer: the
// two's-complement pattern of an integer of that width.
func fmtTruncate(value uint64, bits int) uint64 {
	if bits >= 64 {
		return value
	}
	return value & (1<<uint(bits) - 1)
}

// fmtDebugReflectMap is Debug for a map-like collection (BTreeMap, HashMap,
// IndexMap): `f.debug_map().entries(self.iter()).finish()`. A collection
// whose iteration is not in key order is written in key order.
func fmtDebugReflectMap(m ReflectMap, f *Formatter) error {
	builder := f.DebugMap()
	fmtMapEntries(m, func(key, value any) {
		builder.Entry(key, value)
	})
	return builder.Finish()
}

// fmtMapEntries calls yield with each entry of a collection, sorted by key
// when the collection is not.
func fmtMapEntries(m ReflectMap, yield func(key, value any)) {
	if m.ReflectSorted() {
		m.ReflectEntries(func(key any, value any) bool {
			yield(key, value)
			return true
		})
		return
	}
	type entry struct {
		key   reflect.Value
		value any
	}
	entries := make([]entry, 0, m.ReflectLen())
	m.ReflectEntries(func(key any, value any) bool {
		entries = append(entries, entry{key: reflect.ValueOf(key), value: value})
		return true
	})
	fmtSortStable(len(entries), func(i, j int) bool {
		return fmtCompareKeys(entries[i].key, entries[j].key) < 0
	}, func(i, j int) {
		entries[i], entries[j] = entries[j], entries[i]
	})
	for _, e := range entries {
		yield(e.key.Interface(), e.value)
	}
}

// fmtDebugReflectSeq is Debug for a sequence or a set (VecDeque, BTreeSet,
// HashSet): `f.debug_list()` or `f.debug_set()` over its elements.
func fmtDebugReflectSeq(seq ReflectSeq, f *Formatter) error {
	if seq.ReflectIsSet() {
		builder := f.DebugSet()
		fmtSeqElements(seq, func(element any) {
			builder.Entry(element)
		})
		return builder.Finish()
	}
	builder := f.DebugList()
	fmtSeqElements(seq, func(element any) {
		builder.Entry(element)
	})
	return builder.Finish()
}

// fmtSeqElements calls yield with each element of a collection, sorted when
// the collection is a set whose iteration is not in order.
func fmtSeqElements(seq ReflectSeq, yield func(element any)) {
	if !seq.ReflectIsSet() || seq.ReflectSorted() {
		seq.ReflectElements(func(element any) bool {
			yield(element)
			return true
		})
		return
	}
	type entry struct {
		key     reflect.Value
		element any
	}
	entries := make([]entry, 0, seq.ReflectLen())
	seq.ReflectElements(func(element any) bool {
		key := reflect.ValueOf(element)
		for key.Kind() == reflect.Pointer && !key.IsNil() {
			key = key.Elem()
		}
		entries = append(entries, entry{key: key, element: element})
		return true
	})
	fmtSortStable(len(entries), func(i, j int) bool {
		return fmtCompareKeys(entries[i].key, entries[j].key) < 0
	}, func(i, j int) {
		entries[i], entries[j] = entries[j], entries[i]
	})
	for _, e := range entries {
		yield(e.element)
	}
}

// fmtSortedMapKeys returns a Go map's keys in order.
func fmtSortedMapKeys(m reflect.Value) []reflect.Value {
	keys := m.MapKeys()
	fmtSortStable(len(keys), func(i, j int) bool {
		return fmtCompareKeys(keys[i], keys[j]) < 0
	}, func(i, j int) {
		keys[i], keys[j] = keys[j], keys[i]
	})
	return keys
}

// fmtSortStable sorts n elements in place with Go's stable sort.
func fmtSortStable(n int, less func(i, j int) bool, swap func(i, j int)) {
	sort.Stable(fmtSorter{n: n, less: less, swap: swap})
}

// fmtSorter is sort.Interface over the functions fmtSortStable is given.
type fmtSorter struct {
	n    int
	less func(i, j int) bool
	swap func(i, j int)
}

func (s fmtSorter) Len() int           { return s.n }
func (s fmtSorter) Less(i, j int) bool { return s.less(i, j) }
func (s fmtSorter) Swap(i, j int)      { s.swap(i, j) }

// fmtIsNumberOnlyInGo reports whether a Go type that holds a number stands
// for a Rust type that is not one: a char (rs.Char), a Duration, or a type
// the port registered (a data-free enum, a newtype). Rust has the radix and
// exponent traits for such a type only when its code implements them, and
// then the Go type has the trait's method.
func fmtIsNumberOnlyInGo(t reflect.Type) bool {
	if t == fmtCharType || t == fmtDurationType {
		return true
	}
	return t.Name() != "" && TypeInfoOf(t) != nil
}

// fmtCompareKeys orders two map keys as Rust's Ord orders the types they
// stand for: integers and floats by value, strings by their bytes, false
// before true. Keys of other types are ordered by rs.Cmp.
func fmtCompareKeys(a, b reflect.Value) int {
	for a.Kind() == reflect.Interface && !a.IsNil() {
		a = a.Elem()
	}
	for b.Kind() == reflect.Interface && !b.IsNil() {
		b = b.Elem()
	}
	if a.Kind() == b.Kind() {
		switch a.Kind() {
		case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
			return fmtCompare(a.Int(), b.Int())
		case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
			return fmtCompare(a.Uint(), b.Uint())
		case reflect.Float32, reflect.Float64:
			return fmtCompare(a.Float(), b.Float())
		case reflect.String:
			return fmtCompare(a.String(), b.String())
		case reflect.Bool:
			switch {
			case a.Bool() == b.Bool():
				return 0
			case !a.Bool():
				return -1
			default:
				return 1
			}
		}
	}
	if !a.IsValid() || !b.IsValid() || !a.CanInterface() || !b.CanInterface() {
		return 0
	}
	return Cmp(a.Interface(), b.Interface())
}

func fmtCompare[T int64 | uint64 | float64 | string](a, b T) int {
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	default:
		return 0
	}
}

// fmtRadixValue is `<T as LowerHex>::fmt` and its siblings UpperHex, Octal
// and Binary for a Go value: the integers, and what has the trait's method.
func fmtRadixValue(value any, base int, upper bool, f *Formatter) error {
	switch v := value.(type) {
	case int:
		return fmtRadix(uint64(uint(v)), base, upper, f)
	case int8:
		return fmtRadix(uint64(uint8(v)), base, upper, f)
	case int16:
		return fmtRadix(uint64(uint16(v)), base, upper, f)
	case int32:
		return fmtRadix(uint64(uint32(v)), base, upper, f)
	case int64:
		return fmtRadix(uint64(v), base, upper, f)
	case uint:
		return fmtRadix(uint64(v), base, upper, f)
	case uint8:
		return fmtRadix(uint64(v), base, upper, f)
	case uint16:
		return fmtRadix(uint64(v), base, upper, f)
	case uint32:
		return fmtRadix(uint64(v), base, upper, f)
	case uint64:
		return fmtRadix(v, base, upper, f)
	case uintptr:
		return fmtRadix(uint64(v), base, upper, f)
	}
	var trait, spec string
	switch {
	case base == 2:
		trait, spec = "Binary", "{:b}"
	case base == 8:
		trait, spec = "Octal", "{:o}"
	case upper:
		trait, spec = "UpperHex", "{:X}"
	default:
		trait, spec = "LowerHex", "{:x}"
	}

	v := reflect.ValueOf(value)
	for {
		if !v.IsValid() || fmtIsNil(v) {
			return f.buf.writeStr(fmtNilText)
		}
		// A hand-written impl of the trait.
		target := any(nil)
		if v.CanInterface() {
			target = v.Interface()
		}
		switch {
		case base == 2:
			if impl, ok := target.(Binary); ok {
				return impl.FmtBinary(f)
			}
		case base == 8:
			if impl, ok := target.(Octal); ok {
				return impl.FmtOctal(f)
			}
		case upper:
			if impl, ok := target.(UpperHex); ok {
				return impl.FmtUpperHex(f)
			}
		default:
			if impl, ok := target.(LowerHex); ok {
				return impl.FmtLowerHex(f)
			}
		}
		if wide, ok := target.(fmtInt128); ok {
			hi, lo, _ := wide.fmtInt128()
			return fmtRadixInt128(hi, lo, base, upper, f)
		}
		if fmtIsNumberOnlyInGo(v.Type()) {
			panic(fmtNotImplemented(reflect.ValueOf(value), trait, spec))
		}
		switch v.Kind() {
		case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
			return fmtRadix(fmtTruncate(uint64(v.Int()), v.Type().Bits()), base, upper, f)
		case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
			return fmtRadix(v.Uint(), base, upper, f)
		case reflect.Pointer, reflect.Interface:
			// impl<T: LowerHex> LowerHex for &T
			v = v.Elem()
			continue
		case reflect.Struct, reflect.Array:
			if !v.CanAddr() {
				// The method may have a pointer receiver.
				v = fmtAddressable(v).Addr()
				if fmtImplementsRadix(v.Type(), base, upper) {
					continue
				}
			}
		}
		panic(fmtNotImplemented(reflect.ValueOf(value), trait, spec))
	}
}

func fmtImplementsRadix(t reflect.Type, base int, upper bool) bool {
	switch {
	case base == 2:
		return t.Implements(reflect.TypeFor[Binary]())
	case base == 8:
		return t.Implements(reflect.TypeFor[Octal]())
	case upper:
		return t.Implements(reflect.TypeFor[UpperHex]())
	default:
		return t.Implements(reflect.TypeFor[LowerHex]())
	}
}

// fmtExp is `<T as LowerExp>::fmt` and `<T as UpperExp>::fmt` for a Go
// value: the integers, the floats, and what has the trait's method.
func fmtExp(value any, upper bool, f *Formatter) error {
	letterE := byte('e')
	if upper {
		letterE = 'E'
	}
	switch v := value.(type) {
	case float64:
		return fmtFloatToExponentialCommon(f, fmtFloatOfF64(math.Float64bits(v)), upper)
	case float32:
		return fmtFloatToExponentialCommon(f, fmtFloatOfF32(math.Float32bits(v)), upper)
	case int:
		abs, isNonnegative := fmtAbs64(int64(v))
		return fmtExpU64(f, abs, isNonnegative, letterE)
	case int64:
		abs, isNonnegative := fmtAbs64(v)
		return fmtExpU64(f, abs, isNonnegative, letterE)
	case uint64:
		return fmtExpU64(f, v, true, letterE)
	}
	trait, spec := "LowerExp", "{:e}"
	if upper {
		trait, spec = "UpperExp", "{:E}"
	}

	v := reflect.ValueOf(value)
	for {
		if !v.IsValid() || fmtIsNil(v) {
			return f.buf.writeStr(fmtNilText)
		}
		target := any(nil)
		if v.CanInterface() {
			target = v.Interface()
		}
		if upper {
			if impl, ok := target.(UpperExp); ok {
				return impl.FmtUpperExp(f)
			}
		} else if impl, ok := target.(LowerExp); ok {
			return impl.FmtLowerExp(f)
		}
		if wide, ok := target.(fmtInt128); ok {
			hi, lo, signed := wide.fmtInt128()
			return fmtExpInt128(f, hi, lo, signed, letterE)
		}
		if fmtIsNumberOnlyInGo(v.Type()) {
			panic(fmtNotImplemented(reflect.ValueOf(value), trait, spec))
		}
		switch v.Kind() {
		case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
			abs, isNonnegative := fmtAbs64(v.Int())
			return fmtExpU64(f, abs, isNonnegative, letterE)
		case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
			return fmtExpU64(f, v.Uint(), true, letterE)
		case reflect.Float64:
			return fmtFloatToExponentialCommon(f, fmtFloatOfF64(math.Float64bits(v.Float())), upper)
		case reflect.Float32:
			return fmtFloatToExponentialCommon(f, fmtFloatOfF32(math.Float32bits(float32(v.Float()))), upper)
		case reflect.Pointer, reflect.Interface:
			// impl<T: LowerExp> LowerExp for &T
			v = v.Elem()
			continue
		case reflect.Struct, reflect.Array:
			if !v.CanAddr() {
				// The method may have a pointer receiver.
				v = fmtAddressable(v).Addr()
				expType := reflect.TypeFor[LowerExp]()
				if upper {
					expType = reflect.TypeFor[UpperExp]()
				}
				if v.Type().Implements(expType) {
					continue
				}
			}
		}
		panic(fmtNotImplemented(reflect.ValueOf(value), trait, spec))
	}
}

// fmtPointer is `<T as Pointer>::fmt` for a Go value: `{:p}` prints the
// address a pointer holds. A slice and a string are wide pointers, which Rust
// prints with their length.
func fmtPointer(value any, f *Formatter) error {
	if impl, ok := value.(Pointer); ok {
		return impl.FmtPointer(f)
	}
	v := reflect.ValueOf(value)
	if !v.IsValid() {
		return fmtPointerFmtInner(0, f)
	}
	switch v.Kind() {
	case reflect.Pointer, reflect.UnsafePointer, reflect.Func, reflect.Chan, reflect.Map:
		return fmtPointerFmtInner(v.Pointer(), f)
	case reflect.Slice:
		return fmtWidePointerFmtInner(v.Pointer(), v.Len(), f)
	case reflect.String:
		s := v.String()
		return fmtWidePointerFmtInner(uintptr(unsafe.Pointer(unsafe.StringData(s))), len(s), f)
	}
	panic(fmtNotImplemented(v, "Pointer", "{:p}"))
}
