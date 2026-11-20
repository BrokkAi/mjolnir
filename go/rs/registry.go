package rs

import (
	"reflect"
	"strings"
	"sync"
)

// Go's reflection does not know what a type was called in Rust, which of its
// structs are the variants of one enum, or which serde attributes its
// declaration carried. Each ported type says so once, next to its
// declaration, and the formatter (derived Debug), the structural clone and
// comparison, and go/crates/serde read it from here.
//
//	type RuntimeShutdownReport struct {
//		AwaitsLeft   int  `rs:"awaits_left"`
//		BlockingLeft bool `rs:"blocking_left"`
//	}
//
//	var _ = rs.RegisterStruct[RuntimeShutdownReport]("ShutdownReport")
//
//	type RuntimeBlockingWork interface{ isRuntimeBlockingWork() }
//	type RuntimeBlockingWork_Await struct{}
//	type RuntimeBlockingWork_AwaitFor struct{ F0 time.Duration }
//
//	var _ = rs.RegisterEnum[RuntimeBlockingWork]("BlockingWork",
//		rs.Variant[RuntimeBlockingWork_Await]("Await", rs.ShapeUnit),
//		rs.Variant[RuntimeBlockingWork_AwaitFor]("AwaitFor", rs.ShapeTuple),
//	)

// Shape is how a struct or an enum variant was written in Rust, which decides
// how it prints and how it is serialized.
type Shape uint8

const (
	// ShapeStruct is `Name { field: T }`.
	ShapeStruct Shape = iota
	// ShapeTuple is `Name(T, U)`. The Go fields are F0, F1, ...
	ShapeTuple
	// ShapeUnit is `Name`.
	ShapeUnit
	// ShapeNewtype is `Name(T)` held as a Go defined type over T, such as
	// `type SessionId string`.
	ShapeNewtype
)

// TypeInfo is what a ported type registered about itself.
type TypeInfo struct {
	// RustName is the type's or variant's name in the Rust source.
	RustName string
	// Shape is how it was written.
	Shape Shape
	// GoType is the registered Go type: the struct, the enum's interface, the
	// defined integer type of a data-free enum, or a variant's struct.
	GoType reflect.Type
	// Attrs holds the serde container or variant attributes in serde's own
	// spelling, one entry for each attribute: `tag = "method"`,
	// `rename_all = "snake_case"`, `deny_unknown_fields`.
	Attrs []string
	// Variants lists an enum's variants in declaration order.
	Variants []*TypeInfo
	// Enum is the enum a variant belongs to, and Index its position.
	Enum  *TypeInfo
	Index int
	// Value is a data-free enum's variant as a value of the enum's Go type.
	Value any
	// IsEnum reports whether this entry is an enum (rather than a struct or a
	// variant).
	IsEnum bool
}

// VariantSpec is one variant in a RegisterEnum call.
type VariantSpec struct {
	info *TypeInfo
}

var (
	registryMu sync.RWMutex
	registry   = map[string]*TypeInfo{}
)

// typeKey names a type by package path and base name, without type
// arguments, so every instantiation of a generic type shares one entry.
func typeKey(t reflect.Type) string {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	name := t.Name()
	if bracket := strings.IndexByte(name, '['); bracket >= 0 {
		name = name[:bracket]
	}
	return t.PkgPath() + "." + name
}

func register(info *TypeInfo) *TypeInfo {
	key := typeKey(info.GoType)
	registryMu.Lock()
	defer registryMu.Unlock()
	if existing, ok := registry[key]; ok && existing.RustName != info.RustName {
		panic("rs: " + key + " registered as both " + existing.RustName + " and " + info.RustName)
	}
	registry[key] = info
	return info
}

// RegisterStruct records that T stands for the Rust struct `rustName { .. }`.
// attrs are its serde container attributes. Register one instantiation of a
// generic type.
func RegisterStruct[T any](rustName string, attrs ...string) *TypeInfo {
	return register(&TypeInfo{RustName: rustName, Shape: ShapeStruct, GoType: reflect.TypeFor[T](), Attrs: attrs})
}

// RegisterTupleStruct records that T stands for the Rust tuple struct
// `rustName(T0, T1, ..)`, whose Go fields are F0, F1, ...
func RegisterTupleStruct[T any](rustName string, attrs ...string) *TypeInfo {
	return register(&TypeInfo{RustName: rustName, Shape: ShapeTuple, GoType: reflect.TypeFor[T](), Attrs: attrs})
}

// RegisterUnitStruct records that T stands for the Rust unit struct
// `rustName`.
func RegisterUnitStruct[T any](rustName string, attrs ...string) *TypeInfo {
	return register(&TypeInfo{RustName: rustName, Shape: ShapeUnit, GoType: reflect.TypeFor[T](), Attrs: attrs})
}

// RegisterNewtype records that the defined type T stands for the Rust
// newtype `rustName(Inner)`.
func RegisterNewtype[T any](rustName string, attrs ...string) *TypeInfo {
	return register(&TypeInfo{RustName: rustName, Shape: ShapeNewtype, GoType: reflect.TypeFor[T](), Attrs: attrs})
}

// RegisterEnum records that E stands for the Rust enum rustName with the
// given variants in declaration order. E is the sealed interface of an enum
// whose variants carry data, or the defined integer type of one whose
// variants carry none. Container attributes are passed with EnumAttrs.
func RegisterEnum[E any](rustName string, variants ...VariantSpec) *TypeInfo {
	info := &TypeInfo{RustName: rustName, GoType: reflect.TypeFor[E](), IsEnum: true}
	for _, spec := range variants {
		if spec.info.GoType == nil && spec.info.Value == nil {
			// EnumAttrs: container attributes carried in the variant list.
			info.Attrs = append(info.Attrs, spec.info.Attrs...)
			continue
		}
		spec.info.Enum = info
		spec.info.Index = len(info.Variants)
		info.Variants = append(info.Variants, spec.info)
		if spec.info.GoType != nil {
			register(spec.info)
		}
	}
	return register(info)
}

// EnumAttrs carries an enum's serde container attributes in a RegisterEnum
// call: rs.EnumAttrs(`tag = "method"`, `content = "params"`).
func EnumAttrs(attrs ...string) VariantSpec {
	return VariantSpec{info: &TypeInfo{Attrs: attrs}}
}

// Variant describes the variant of a data-carrying enum that the struct V
// stands for. attrs are the variant's serde attributes.
func Variant[V any](rustName string, shape Shape, attrs ...string) VariantSpec {
	return VariantSpec{info: &TypeInfo{RustName: rustName, Shape: shape, GoType: reflect.TypeFor[V](), Attrs: attrs}}
}

// Const describes one variant of a data-free enum: its constant and its Rust
// name.
func Const[E any](value E, rustName string, attrs ...string) VariantSpec {
	return VariantSpec{info: &TypeInfo{RustName: rustName, Shape: ShapeUnit, Value: value, Attrs: attrs}}
}

// TypeInfoOf returns what the type registered, or nil. A pointer type is
// looked up by the type it points to.
func TypeInfoOf(t reflect.Type) *TypeInfo {
	if t == nil {
		return nil
	}
	registryMu.RLock()
	defer registryMu.RUnlock()
	return registry[typeKey(t)]
}

// VariantOf returns the registered variant that a data-free enum value is,
// or nil.
func VariantOf(value any) *TypeInfo {
	info := TypeInfoOf(reflect.TypeOf(value))
	if info == nil || !info.IsEnum {
		return nil
	}
	for _, variant := range info.Variants {
		if variant.Value != nil && reflect.DeepEqual(variant.Value, value) {
			return variant
		}
	}
	return nil
}

// Attr returns the value of the attribute name in attrs and whether it is
// present: Attr(`tag = "method"`, "tag") is ("method", true), and a bare
// attribute such as `deny_unknown_fields` has an empty value.
func Attr(attrs []string, name string) (string, bool) {
	for _, attr := range attrs {
		key, value, hasValue := strings.Cut(attr, "=")
		if strings.TrimSpace(key) != name {
			continue
		}
		if !hasValue {
			return "", true
		}
		return strings.Trim(strings.TrimSpace(value), `"`), true
	}
	return "", false
}

// FieldRustName returns a struct field's Rust name from its `rs` tag, and the
// tag's options (such as "arc"). A field without the tag has no Rust name:
// its Go name is returned.
func FieldRustName(field reflect.StructField) (name string, options []string) {
	tag, ok := field.Tag.Lookup("rs")
	if !ok {
		return field.Name, nil
	}
	parts := strings.Split(tag, ",")
	if parts[0] == "" {
		return field.Name, parts[1:]
	}
	return parts[0], parts[1:]
}
