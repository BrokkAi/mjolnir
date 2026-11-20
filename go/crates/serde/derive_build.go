package serde

import (
	"reflect"
	"strings"
	"sync"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// derivedStruct is a struct's derived Serialize and Deserialize.
type derivedStruct struct {
	fields *fieldSet
	// into and from are the `#[serde(into)]` and `#[serde(from)]` or
	// `#[serde(try_from)]` conversions.
	into func(p unsafe.Pointer, serializer Serializer) (any, error)
	from func(p unsafe.Pointer, deserializer Deserializer) error
}

// derivedEnum is an enum's derived Serialize and Deserialize.
type derivedEnum struct {
	// typ is the enum's Go type: its interface, or the integer type of a
	// data-free enum.
	typ      reflect.Type
	cont     *containerAttrs
	typeName string
	where    string
	variants []*variant
	// deVariants are the variants that are deserialized, and variantNames
	// is `VARIANTS`: their names and aliases.
	deVariants   []*variant
	variantNames []string
	// byType finds the variant of an enum held as an interface by the type
	// of the pointer it holds; byValue finds that of a data-free enum.
	byType  map[reflect.Type]*variant
	byValue map[uint64]*variant
	// For an enum with untagged variants after tagged ones: the enum of
	// the tagged variants alone, tried first, and the untagged variants.
	firstUntagged    int
	tagged           *derivedEnum
	untaggedVariants []*variant
	// ordinary is the variants of a field or variant identifier.
	ordinary []*variant
	into     func(p unsafe.Pointer, serializer Serializer) (any, error)
	from     func(p unsafe.Pointer, deserializer Deserializer) error
}

// buildDerived fills in the codec of a type whose Serialize and Deserialize
// serde derives. info is the type's registration, or nil for an
// unregistered struct, which is derived with its Go field names.
func buildDerived(c *codec, info *rs.TypeInfo) {
	if info != nil && info.IsEnum {
		buildDerivedEnum(c, info)
		return
	}
	typ := c.typ
	rustName, shape, attrs := typ.Name(), rs.ShapeStruct, []string(nil)
	if bracket := strings.IndexByte(rustName, '['); bracket >= 0 {
		rustName = rustName[:bracket]
	}
	if info != nil {
		rustName, shape, attrs = info.RustName, info.Shape, info.Attrs
	}
	where := typ.String()
	cont := parseContainerAttrs(rustName, attrs, where, false)
	newtype := shape == rs.ShapeNewtype
	st := styleNewtype
	if !newtype {
		if typ.Kind() != reflect.Struct {
			panic(rs.Panicf("serde: " + where + " is registered as a struct but is a Go " + typ.Kind().String()))
		}
		st = shapeStyle(shape, typ.NumField())
	}
	fs := buildFieldSet(typ, st, &cont, nil, unrawIdent(rustName), where, newtype)
	if cont.transparent {
		fs.markTransparent()
	}
	fs.resolveFunctions()
	d := &derivedStruct{fields: fs}
	d.into, d.from = conversions(typ, &cont, where)
	c.ser, c.de = d.serialize, d.deserialize
}

// buildDerivedEnum builds the codec of a registered enum.
func buildDerivedEnum(c *codec, info *rs.TypeInfo) {
	typ := c.typ
	where := typ.String()
	cont := parseContainerAttrs(info.RustName, info.Attrs, where, true)
	e := &derivedEnum{
		typ:           typ,
		cont:          &cont,
		typeName:      unrawIdent(info.RustName),
		where:         where,
		byType:        map[reflect.Type]*variant{},
		byValue:       map[uint64]*variant{},
		firstUntagged: -1,
	}
	for index, vi := range info.Variants {
		variantWhere := where + "::" + vi.RustName
		v := &variant{index: index, ident: vi.RustName, attrs: parseVariantAttrs(vi.RustName, vi.Attrs, variantWhere), where: variantWhere}
		if vi.GoType != nil {
			goType := vi.GoType
			for goType.Kind() == reflect.Pointer {
				goType = goType.Elem()
			}
			if goType.Kind() != reflect.Struct {
				panic(rs.Panicf("serde: the variant " + variantWhere + " is held in a Go " + goType.Kind().String() + ", not a struct"))
			}
			v.goType = goType
			v.style = shapeStyle(vi.Shape, goType.NumField())
			v.fields = buildFieldSet(goType, v.style, &cont, &v.attrs, e.typeName, variantWhere, false)
			v.fields.resolveFunctions()
			e.byType[reflect.PointerTo(goType)] = v
		} else {
			if typ.Kind() == reflect.Interface {
				panic(rs.Panicf("serde: the variant " + variantWhere + " of an enum held as an interface has no struct"))
			}
			v.style = styleUnit
			v.value = integerValue(reflect.ValueOf(vi.Value))
			v.fields = &fieldSet{style: styleUnit, typ: typ, cont: &cont, typeName: e.typeName, where: variantWhere}
			e.byValue[v.value] = v
		}
		v.attrs.name.renameVariantByRules(cont.renameAll)
		if v.attrs.serializeWith != "" {
			v.serWith = lookupFn(fnSerializeWith, v.attrs.serializeWith, v.holder(), variantWhere).ser
		}
		if v.attrs.deserializeWith != "" {
			v.deWith = lookupFn(fnDeserializeWith, v.attrs.deserializeWith, v.holder(), variantWhere).de
		}
		e.variants = append(e.variants, v)
		if v.attrs.untagged && e.firstUntagged < 0 {
			e.firstUntagged = index
		}
	}
	if e.firstUntagged >= 0 {
		for _, v := range e.variants[e.firstUntagged:] {
			if !v.attrs.untagged {
				panic(rs.Panicf("serde: " + where + ": all variants with the #[serde(untagged)] attribute must be placed at the end of the enum"))
			}
		}
	}
	if cont.tag == tagInternal {
		for _, v := range e.variants {
			if v.style == styleTuple {
				panic(rs.Panicf("serde: " + where + `: #[serde(tag = "...")] cannot be used with tuple variants`))
			}
		}
	}
	e.prepare(e.variants)
	if e.firstUntagged >= 0 {
		tagged := *e
		tagged.firstUntagged = -1
		tagged.prepare(e.variants[:e.firstUntagged])
		e.tagged = &tagged
		for _, v := range e.variants[e.firstUntagged:] {
			if !v.attrs.skipDeserializing {
				e.untaggedVariants = append(e.untaggedVariants, v)
			}
		}
	}
	e.into, e.from = conversions(typ, &cont, where)
	c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		if e.cont.identifier != identifierNo {
			panic(rs.Panicf("serde: " + where + ": field and variant identifiers cannot be serialized"))
		}
		if e.into != nil {
			return e.into(p, serializer)
		}
		v, base := e.variantAt(p)
		return e.serializeVariant(v, base, serializer)
	}
	c.de = e.deserialize
	if typ.Kind() == reflect.Interface {
		c.serVariant = func(variantType reflect.Type, p unsafe.Pointer, serializer Serializer) (any, error) {
			if e.into != nil {
				holder := reflect.New(typ)
				holder.Elem().Set(reflect.NewAt(variantType, p))
				return e.into(holder.UnsafePointer(), serializer)
			}
			v := e.byType[reflect.PointerTo(variantType)]
			if v == nil {
				panic(rs.Panicf("serde: " + variantType.String() + " is not a variant of " + where))
			}
			return e.serializeVariant(v, p, serializer)
		}
	}
}

// holder is the Go type a variant's serialize_with and deserialize_with
// functions take: the variant's struct, or the enum's type for a data-free
// enum.
func (v *variant) holder() reflect.Type {
	if v.goType != nil {
		return v.goType
	}
	return v.fields.typ
}

// prepare sets the variants an enum deserializes and their names.
func (e *derivedEnum) prepare(variants []*variant) {
	e.deVariants, e.variantNames = nil, nil
	for _, v := range variants {
		if v.attrs.skipDeserializing {
			continue
		}
		e.deVariants = append(e.deVariants, v)
		e.variantNames = append(e.variantNames, v.attrs.name.aliases...)
	}
}

// variantAt returns the variant of the enum value at p and the address of
// its data: the variant's struct, or p itself for a data-free enum.
func (e *derivedEnum) variantAt(p unsafe.Pointer) (*variant, unsafe.Pointer) {
	value := reflect.NewAt(e.typ, p).Elem()
	if e.typ.Kind() != reflect.Interface {
		v := e.byValue[integerValue(value)]
		if v == nil {
			panic(rs.Panicf("serde: " + strings.TrimSpace(value.String()) + " is not a variant of " + e.where))
		}
		return v, p
	}
	if value.IsNil() {
		panic(rs.Panicf("serde: cannot serialize a nil " + e.where + "; nothing in Rust is nil"))
	}
	held := value.Elem()
	v := e.byType[held.Type()]
	if v == nil {
		panic(rs.Panicf("serde: " + held.Type().String() + " is not a variant of " + e.where + "; an enum value is a pointer to one of its variant structs"))
	}
	if held.IsNil() {
		panic(rs.Panicf("serde: cannot serialize a nil " + held.Type().String() + "; nothing in Rust is nil"))
	}
	return v, held.UnsafePointer()
}

// conversion is a function registered for `#[serde(from)]`,
// `#[serde(try_from)]` or `#[serde(into)]`.
type conversion struct {
	ser func(p unsafe.Pointer, serializer Serializer) (any, error)
	de  func(p unsafe.Pointer, deserializer Deserializer) error
}

var (
	conversionsMu sync.RWMutex
	intoFns       = map[reflect.Type]conversion{}
	fromFns       = map[reflect.Type]conversion{}
	tryFromFns    = map[reflect.Type]conversion{}
)

// RegisterFrom registers the conversion of `#[serde(from = "U")]` on T: T
// deserializes as a U, which `T::from` converts. It returns true so that it
// can initialise a package variable.
//
//	var _ = serde.RegisterFrom(ReviewConfig_from_ReviewConfigInput)
func RegisterFrom[T any, U any](from func(U) T) bool {
	conversionsMu.Lock()
	defer conversionsMu.Unlock()
	fromFns[reflect.TypeFor[T]()] = conversion{de: func(p unsafe.Pointer, deserializer Deserializer) error {
		value, err := Deserialize[U](deserializer)
		if err != nil {
			return err
		}
		*(*T)(p) = from(value)
		return nil
	}}
	return true
}

// RegisterTryFrom registers the conversion of `#[serde(try_from = "U")]` on
// T: T deserializes as a U, which `T::try_from` converts; its error becomes
// `de::Error::custom(error)`.
func RegisterTryFrom[T any, U any](tryFrom func(U) (T, error)) bool {
	conversionsMu.Lock()
	defer conversionsMu.Unlock()
	tryFromFns[reflect.TypeFor[T]()] = conversion{de: func(p unsafe.Pointer, deserializer Deserializer) error {
		value, err := Deserialize[U](deserializer)
		if err != nil {
			return err
		}
		converted, err := tryFrom(value)
		if err != nil {
			return DeErrorCustom(err)
		}
		*(*T)(p) = converted
		return nil
	}}
	return true
}

// RegisterInto registers the conversion of `#[serde(into = "U")]` on T: T
// serializes as the U that `T::into` makes of a clone of it. The function
// receives a copy of the value; one that changes what the copy shares must
// clone it first (rs.Clone), as Rust's clone would have.
func RegisterInto[T any, U any](into func(T) U) bool {
	conversionsMu.Lock()
	defer conversionsMu.Unlock()
	intoFns[reflect.TypeFor[T]()] = conversion{ser: func(p unsafe.Pointer, serializer Serializer) (any, error) {
		converted := into(*(*T)(p))
		return SerializeRef(&converted, serializer)
	}}
	return true
}

// conversions returns the into and from functions the container attributes
// ask for.
func conversions(typ reflect.Type, cont *containerAttrs, where string) (into func(unsafe.Pointer, Serializer) (any, error), from func(unsafe.Pointer, Deserializer) error) {
	conversionsMu.RLock()
	defer conversionsMu.RUnlock()
	if cont.typeInto != "" {
		found, ok := intoFns[typ]
		if !ok {
			panic(rs.Panicf("serde: " + where + ": #[serde(into = \"" + cont.typeInto + "\")] needs the conversion registered with serde.RegisterInto"))
		}
		into = found.ser
	}
	switch {
	case cont.typeFrom != "" && cont.typeTryFrom != "":
		panic(rs.Panicf("serde: " + where + `: #[serde(from = "...")] and #[serde(try_from = "...")] conflict with each other`))
	case cont.typeFrom != "":
		found, ok := fromFns[typ]
		if !ok {
			panic(rs.Panicf("serde: " + where + ": #[serde(from = \"" + cont.typeFrom + "\")] needs the conversion registered with serde.RegisterFrom"))
		}
		from = found.de
	case cont.typeTryFrom != "":
		found, ok := tryFromFns[typ]
		if !ok {
			panic(rs.Panicf("serde: " + where + ": #[serde(try_from = \"" + cont.typeTryFrom + "\")] needs the conversion registered with serde.RegisterTryFrom"))
		}
		from = found.de
	}
	return into, from
}
