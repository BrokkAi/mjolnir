// Port of serde_derive 1.0.229 src/internals/ast.rs, src/internals/attr.rs
// and src/internals/check.rs: what a derive learns from the type's
// declaration, here read from its registration (rs.RegisterStruct,
// rs.RegisterEnum, ...) and its struct tags.

package serde

import (
	"reflect"
	"strconv"
	"strings"
	"sync/atomic"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// style is `ast::Style`: how a struct or a variant holds its fields.
type style uint8

const (
	// styleStruct is named fields.
	styleStruct style = iota
	// styleTuple is many unnamed fields.
	styleTuple
	// styleNewtype is one unnamed field.
	styleNewtype
	// styleUnit is no fields.
	styleUnit
)

// defaultKind is `attr::Default`: the default to use for a field when
// deserializing.
type defaultKind uint8

const (
	// defaultNone is a field that must always be specified because it does
	// not have a default.
	defaultNone defaultKind = iota
	// defaultDefault is `std::default::Default::default()`.
	defaultDefault
	// defaultPath is the default given by a function.
	defaultPath
)

type defaultAttr struct {
	kind defaultKind
	path string
}

// tagType is `attr::TagType`: how an enum is represented.
type tagType uint8

const (
	// tagExternal is the default: {"variant1": {"key1": "value1"}}.
	tagExternal tagType = iota
	// tagInternal is `#[serde(tag = "type")]`.
	tagInternal
	// tagAdjacent is `#[serde(tag = "t", content = "c")]`.
	tagAdjacent
	// tagNone is `#[serde(untagged)]`.
	tagNone
)

// identifierKind is `attr::Identifier`: whether an enum represents the
// fields of a struct or the variants of an enum.
type identifierKind uint8

const (
	identifierNo identifierKind = iota
	identifierField
	identifierVariant
)

// containerAttrs is `attr::Container`.
type containerAttrs struct {
	name              multiName
	transparent       bool
	denyUnknownFields bool
	dflt              defaultAttr
	renameAll         renameAllRules
	renameAllFields   renameAllRules
	tag               tagType
	tagName           string
	contentName       string
	typeFrom          string
	typeTryFrom       string
	typeInto          string
	identifier        identifierKind
	expecting         string
	hasExpecting      bool
}

// variantAttrs is `attr::Variant`.
type variantAttrs struct {
	name              multiName
	renameAll         renameAllRules
	skipDeserializing bool
	skipSerializing   bool
	other             bool
	serializeWith     string
	deserializeWith   string
	untagged          bool
}

// fieldAttrs is `attr::Field`.
type fieldAttrs struct {
	name              multiName
	skipSerializing   bool
	skipDeserializing bool
	skipSerializingIf string
	dflt              defaultAttr
	serializeWith     string
	deserializeWith   string
	flatten           bool
	// transparentSer and transparentDe mark the field a transparent
	// container serializes or deserializes as itself; serde picks it
	// separately for each derive.
	transparentSer bool
	transparentDe  bool
	// rust is the field's Rust type, from the `rust=` entry of its tag.
	rust *rustType
}

func attributeString(attr attribute, where string) string {
	if !attr.hasValue {
		panic(rs.Panicf("serde: " + where + ": #[serde(" + attr.name + ")] needs a value"))
	}
	return attr.value
}

// parseContainerAttrs is `attr::Container::from_ast`.
func parseContainerAttrs(rustName string, entries []string, where string, isEnum bool) containerAttrs {
	var attrs []attribute
	for _, entry := range entries {
		attrs = append(attrs, parseAttributes(entry, where)...)
	}
	cont := containerAttrs{name: newMultiName(rustName, attrs)}
	// A container has no aliases.
	cont.name.aliases = nil
	untagged, hasTag, hasContent := false, false, false
	fieldIdentifier, variantIdentifier := false, false
	for _, attr := range attrs {
		switch attr.name {
		case "rename", "bound", "crate", "remote_unused":
		case "rename_all":
			cont.renameAll = cont.renameAll.or(parseRenameAll([]attribute{attr}, "rename_all", where))
		case "rename_all_fields":
			if !isEnum {
				panic(rs.Panicf("serde: " + where + ": #[serde(rename_all_fields)] can only be used on enums"))
			}
			cont.renameAllFields = cont.renameAllFields.or(parseRenameAll([]attribute{attr}, "rename_all_fields", where))
		case "transparent":
			cont.transparent = true
		case "deny_unknown_fields":
			cont.denyUnknownFields = true
		case "default":
			if isEnum {
				panic(rs.Panicf("serde: " + where + ": #[serde(default)] can only be used on structs"))
			}
			if attr.hasValue {
				cont.dflt = defaultAttr{kind: defaultPath, path: attr.value}
			} else {
				cont.dflt = defaultAttr{kind: defaultDefault}
			}
		case "untagged":
			untagged = true
		case "tag":
			hasTag = true
			cont.tagName = attributeString(attr, where)
		case "content":
			hasContent = true
			cont.contentName = attributeString(attr, where)
		case "from":
			cont.typeFrom = attributeString(attr, where)
		case "try_from":
			cont.typeTryFrom = attributeString(attr, where)
		case "into":
			cont.typeInto = attributeString(attr, where)
		case "field_identifier":
			fieldIdentifier = true
		case "variant_identifier":
			variantIdentifier = true
		case "expecting":
			cont.expecting, cont.hasExpecting = attributeString(attr, where), true
		case "remote":
			panic(rs.Todo("serde_derive: #[serde(remote)] on " + where + ": Mjolnir does not use it"))
		default:
			panic(rs.Panicf("serde: " + where + ": unknown serde container attribute `" + attr.name + "`"))
		}
	}
	// decide_tag
	switch {
	case !untagged && !hasTag && !hasContent:
		cont.tag = tagExternal
	case untagged && !hasTag && !hasContent:
		cont.tag = tagNone
	case !untagged && hasTag && !hasContent:
		cont.tag = tagInternal
	case !untagged && hasTag && hasContent:
		cont.tag = tagAdjacent
	default:
		panic(rs.Panicf("serde: " + where + ": conflicting #[serde(untagged)], #[serde(tag)] and #[serde(content)]"))
	}
	// decide_identifier
	switch {
	case fieldIdentifier && variantIdentifier:
		panic(rs.Panicf("serde: " + where + ": #[serde(field_identifier)] and #[serde(variant_identifier)] cannot both be set"))
	case fieldIdentifier:
		cont.identifier = identifierField
	case variantIdentifier:
		cont.identifier = identifierVariant
	}
	if (fieldIdentifier || variantIdentifier) && !isEnum {
		panic(rs.Panicf("serde: " + where + ": #[serde(field_identifier)] and #[serde(variant_identifier)] can only be used on an enum"))
	}
	return cont
}

// parseVariantAttrs is `attr::Variant::from_ast`.
func parseVariantAttrs(rustName string, entries []string, where string) variantAttrs {
	var attrs []attribute
	for _, entry := range entries {
		attrs = append(attrs, parseAttributes(entry, where)...)
	}
	v := variantAttrs{name: newMultiName(unraw(rustName), attrs)}
	for _, attr := range attrs {
		switch attr.name {
		case "rename", "alias", "bound", "borrow":
		case "rename_all":
			v.renameAll = v.renameAll.or(parseRenameAll([]attribute{attr}, "rename_all", where))
		case "skip":
			v.skipSerializing, v.skipDeserializing = true, true
		case "skip_deserializing":
			v.skipDeserializing = true
		case "skip_serializing":
			v.skipSerializing = true
		case "other":
			v.other = true
		case "with":
			path := attributeString(attr, where)
			v.serializeWith, v.deserializeWith = path+"::serialize", path+"::deserialize"
		case "serialize_with":
			v.serializeWith = attributeString(attr, where)
		case "deserialize_with":
			v.deserializeWith = attributeString(attr, where)
		case "untagged":
			v.untagged = true
		default:
			panic(rs.Panicf("serde: " + where + ": unknown serde variant attribute `" + attr.name + "`"))
		}
	}
	return v
}

// parseFieldAttrs is `attr::Field::from_ast`. sourceName is the field's
// Rust name, or its index for a tuple field.
func parseFieldAttrs(sourceName string, tag string, where string, containerDefault defaultAttr) fieldAttrs {
	attrs := parseAttributes(tag, where)
	f := fieldAttrs{name: newMultiName(unraw(sourceName), attrs)}
	for _, attr := range attrs {
		switch attr.name {
		case "rename", "alias", "bound", "borrow":
		case "default":
			if attr.hasValue {
				f.dflt = defaultAttr{kind: defaultPath, path: attr.value}
			} else {
				f.dflt = defaultAttr{kind: defaultDefault}
			}
		case "skip_serializing":
			f.skipSerializing = true
		case "skip_deserializing":
			f.skipDeserializing = true
		case "skip":
			f.skipSerializing, f.skipDeserializing = true, true
		case "skip_serializing_if":
			f.skipSerializingIf = attributeString(attr, where)
		case "serialize_with":
			f.serializeWith = attributeString(attr, where)
		case "deserialize_with":
			f.deserializeWith = attributeString(attr, where)
		case "with":
			path := attributeString(attr, where)
			f.serializeWith, f.deserializeWith = path+"::serialize", path+"::deserialize"
		case "flatten":
			f.flatten = true
		case "getter":
			panic(rs.Todo("serde_derive: #[serde(getter)] on " + where + ": only remote derives have getters"))
		case "rust":
			f.rust = parseRustType(attributeString(attr, where))
			if f.rust == nil {
				panic(rs.Panicf("serde: " + where + ": cannot read the Rust type `" + attr.value + "`"))
			}
		default:
			panic(rs.Panicf("serde: " + where + ": unknown serde field attribute `" + attr.name + "`"))
		}
	}
	// If skip_deserializing, initialize the field to Default::default()
	// unless a different default is specified by `#[serde(default = "...")]`
	// on ourselves or our container (e.g. the struct we are in).
	if containerDefault.kind == defaultNone && f.skipDeserializing && f.dflt.kind == defaultNone {
		f.dflt = defaultAttr{kind: defaultDefault}
	}
	return f
}

// field is `ast::Field`, with what the engine needs to reach and convert
// the field's value.
type field struct {
	// member is the Rust member: the field's name, or its index.
	member string
	// offset is where the field lives in its struct; typ is its Go type.
	offset uintptr
	typ    reflect.Type
	attrs  fieldAttrs
	// where names the field in panics.
	where string

	// The functions the attributes name, resolved when the codec is built.
	skipIf     func(p unsafe.Pointer) bool
	serWith    func(p unsafe.Pointer, serializer Serializer) (any, error)
	deWith     func(p unsafe.Pointer, deserializer Deserializer) error
	setDefault func(p unsafe.Pointer)

	// codec is built on first use: a field whose direction is skipped or
	// handled by a function need not have one, as in Rust, where the bound
	// is not generated.
	codecCache atomic.Pointer[codec]
}

func (f *field) codec() *codec {
	if c := f.codecCache.Load(); c != nil {
		return c
	}
	c := codecFor(f.typ, f.attrs.rust)
	f.codecCache.Store(c)
	return c
}

// pointer returns the address of the field in the value at base.
func (f *field) pointer(base unsafe.Pointer) unsafe.Pointer { return unsafe.Add(base, f.offset) }

// serValue is what the field is handed to a Serializer as: its address
// with its codec, or with its serialize_with function.
func (f *field) serValue(base unsafe.Pointer) any {
	p := f.pointer(base)
	if f.serWith != nil {
		return &serializeWithRef{serialize: f.serWith, pointer: p}
	}
	return &serializeRef{codec: f.codec(), pointer: p}
}

// skipped reports whether skip_serializing_if says to leave the field out.
func (f *field) skipped(base unsafe.Pointer) bool {
	return f.skipIf != nil && f.skipIf(f.pointer(base))
}

// deserialize reads the field from the deserializer into its place in the
// value at base, with its deserialize_with function when it has one.
func (f *field) deserialize(base unsafe.Pointer, deserializer Deserializer) error {
	p := f.pointer(base)
	if f.deWith != nil {
		return f.deWith(p, deserializer)
	}
	return f.codec().de(p, deserializer)
}

// seed is the field as a DeserializeSeed into the value at base.
func (f *field) seed(base unsafe.Pointer) DeDeserializeSeed {
	return &fieldSeed{f: f, base: base}
}

type fieldSeed struct {
	f    *field
	base unsafe.Pointer
}

func (s *fieldSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	return nil, s.f.deserialize(s.base, deserializer)
}

// serializeWithRef hands a field with a serialize_with function to a
// Serializer: `__SerializeWith`.
type serializeWithRef struct {
	serialize func(p unsafe.Pointer, serializer Serializer) (any, error)
	pointer   unsafe.Pointer
}

func (r *serializeWithRef) SerializeSerde(serializer Serializer) (any, error) {
	return r.serialize(r.pointer, serializer)
}

// fieldSet is the fields of a struct or of one enum variant, and how the
// container they belong to treats them.
type fieldSet struct {
	style  style
	fields []*field
	// typ is the Go type holding the fields: the struct, or the variant's
	// struct.
	typ reflect.Type
	// cont is the attributes of the container: the struct, or the enum.
	cont *containerAttrs
	// containerDefault stores the container's Default (or default = path)
	// in a value of typ. Only a struct has one.
	containerDefault func(p unsafe.Pointer)
	// typeName is `params.type_name()`, the container's Rust identifier.
	typeName string
	where    string

	// deFields are the fields a map key can name: neither skipped when
	// deserializing nor flattened. fieldNames is `FIELDS`: their names and
	// aliases.
	deFields   []*field
	fieldNames []string
}

// hasFlattenDe is `has_flatten`: a flattened field that is deserialized.
func (fs *fieldSet) hasFlattenDe() bool {
	for _, f := range fs.fields {
		if f.attrs.flatten && !f.attrs.skipDeserializing {
			return true
		}
	}
	return false
}

// hasFlattenSer is whether a flattened field is serialized.
func (fs *fieldSet) hasFlattenSer() bool {
	for _, f := range fs.fields {
		if f.attrs.flatten && !f.attrs.skipSerializing {
			return true
		}
	}
	return false
}

// newtypeInners holds the inner type of each newtype struct whose Go type
// is a defined type over a struct or an interface, which reflection cannot
// recover (RegisterNewtypeInner).
var newtypeInners atomic.Pointer[map[reflect.Type]reflect.Type]

// RegisterNewtypeInner records that the defined type N stands for the Rust
// newtype struct `N(Inner)`. The engine needs it when Inner is a struct or
// an enum of the port: `type TranscriptPosition SavedTranscriptAnchor`
// keeps the fields but not the registration of SavedTranscriptAnchor,
// which says how they are named. It returns true so that it can initialise
// a package variable.
//
//	var _ = serde.RegisterNewtypeInner[TranscriptPosition, SavedTranscriptAnchor]()
func RegisterNewtypeInner[N any, Inner any]() bool {
	newtype, inner := reflect.TypeFor[N](), reflect.TypeFor[Inner]()
	if newtype.Size() != inner.Size() || !inner.ConvertibleTo(newtype) {
		panic(rs.Panicf("serde.RegisterNewtypeInner: " + newtype.String() + " is not a defined type over " + inner.String()))
	}
	for {
		old := newtypeInners.Load()
		next := map[reflect.Type]reflect.Type{}
		if old != nil {
			for k, v := range *old {
				next[k] = v
			}
		}
		next[newtype] = inner
		if newtypeInners.CompareAndSwap(old, &next) {
			return true
		}
	}
}

// newtypeInner returns the Go type that holds the one field of the newtype
// struct typ, which is a Go defined type over it.
func newtypeInner(typ reflect.Type, where string) reflect.Type {
	if registered := newtypeInners.Load(); registered != nil {
		if inner, ok := (*registered)[typ]; ok {
			return inner
		}
	}
	switch typ.Kind() {
	case reflect.Bool:
		return reflect.TypeFor[bool]()
	case reflect.Int:
		return reflect.TypeFor[int]()
	case reflect.Int8:
		return reflect.TypeFor[int8]()
	case reflect.Int16:
		return reflect.TypeFor[int16]()
	case reflect.Int32:
		return reflect.TypeFor[int32]()
	case reflect.Int64:
		return reflect.TypeFor[int64]()
	case reflect.Uint:
		return reflect.TypeFor[uint]()
	case reflect.Uint8:
		return reflect.TypeFor[uint8]()
	case reflect.Uint16:
		return reflect.TypeFor[uint16]()
	case reflect.Uint32:
		return reflect.TypeFor[uint32]()
	case reflect.Uint64:
		return reflect.TypeFor[uint64]()
	case reflect.Float32:
		return reflect.TypeFor[float32]()
	case reflect.Float64:
		return reflect.TypeFor[float64]()
	case reflect.String:
		return reflect.TypeFor[string]()
	case reflect.Slice:
		return reflect.SliceOf(typ.Elem())
	case reflect.Array:
		return reflect.ArrayOf(typ.Len(), typ.Elem())
	case reflect.Pointer:
		return reflect.PointerTo(typ.Elem())
	}
	panic(rs.Panicf("serde: the newtype " + where + " is a Go defined type over a " + typ.Kind().String() +
		", whose own type reflection cannot recover; add `var _ = serde.RegisterNewtypeInner[" + typ.Name() + ", <inner type>]()` next to it"))
}

// buildFieldSet reads the fields of the struct type typ (a container or an
// enum variant) with their attributes. newtype says typ is a Go defined type
// over the newtype's one field.
func buildFieldSet(typ reflect.Type, st style, cont *containerAttrs, variant *variantAttrs, typeName string, where string, newtype bool) *fieldSet {
	fs := &fieldSet{style: st, typ: typ, cont: cont, typeName: typeName, where: where}
	switch {
	case newtype:
		inner := newtypeInner(typ, where)
		fs.fields = []*field{{member: "0", offset: 0, typ: inner, attrs: parseFieldAttrs("0", "", where+".0", cont.dflt), where: where + ".0"}}
	case st != styleUnit:
		for index := range typ.NumField() {
			goField := typ.Field(index)
			member := strconv.Itoa(index)
			if st == styleStruct {
				member, _ = rs.FieldRustName(goField)
			}
			fieldWhere := where + "." + member
			f := &field{
				member: member,
				offset: goField.Offset,
				typ:    goField.Type,
				attrs:  parseFieldAttrs(member, goField.Tag.Get("serde"), fieldWhere, cont.dflt),
				where:  fieldWhere,
			}
			fs.fields = append(fs.fields, f)
		}
	}
	// Container::from_ast: rename the fields by the rules.
	rules := cont.renameAll
	if variant != nil {
		rules = variant.renameAll.or(cont.renameAllFields)
	}
	for _, f := range fs.fields {
		f.attrs.name.renameFieldByRules(rules)
	}
	for _, f := range fs.fields {
		if !f.attrs.skipDeserializing && !f.attrs.flatten {
			fs.deFields = append(fs.deFields, f)
			fs.fieldNames = append(fs.fieldNames, f.attrs.name.aliases...)
		}
	}
	return fs
}

// resolveFunctions looks up the functions the field attributes name.
func (fs *fieldSet) resolveFunctions() {
	for _, f := range fs.fields {
		if f.attrs.skipSerializingIf != "" {
			f.skipIf = lookupFn(fnSkipSerializingIf, f.attrs.skipSerializingIf, f.typ, f.where).skip
		}
		if f.attrs.serializeWith != "" {
			f.serWith = lookupFn(fnSerializeWith, f.attrs.serializeWith, f.typ, f.where).ser
		}
		if f.attrs.deserializeWith != "" {
			f.deWith = lookupFn(fnDeserializeWith, f.attrs.deserializeWith, f.typ, f.where).de
		}
		switch f.attrs.dflt.kind {
		case defaultDefault:
			f.setDefault = defaultSetter(f.typ, "#[serde(default)] on "+f.where)
		case defaultPath:
			f.setDefault = lookupFn(fnDefault, f.attrs.dflt.path, f.typ, f.where).setDef
		}
	}
	switch fs.cont.dflt.kind {
	case defaultDefault:
		fs.containerDefault = defaultSetter(fs.typ, "#[serde(default)] on "+fs.where)
	case defaultPath:
		fs.containerDefault = lookupFn(fnDefault, fs.cont.dflt.path, fs.typ, fs.where).setDef
	}
}

// markTransparent is `check_transparent`: it picks the field a transparent
// struct serializes as, and the one it deserializes as.
func (fs *fieldSet) markTransparent() {
	var ser, de *field
	for _, f := range fs.fields {
		// PhantomData is never the transparent field; the port holds it as
		// rs.Unit.
		phantom := f.typ == unitType
		if !phantom && !f.attrs.skipSerializing {
			if ser != nil {
				panic(rs.Panicf("serde: " + fs.where + ": #[serde(transparent)] requires struct to have at most one transparent field"))
			}
			ser = f
		}
		if !phantom && !f.attrs.skipDeserializing && f.attrs.dflt.kind == defaultNone {
			if de != nil {
				panic(rs.Panicf("serde: " + fs.where + ": #[serde(transparent)] requires struct to have at most one transparent field"))
			}
			de = f
		}
	}
	if ser != nil {
		ser.attrs.transparentSer = true
	}
	if de != nil {
		de.attrs.transparentDe = true
	}
}

// variant is `ast::Variant`.
type variant struct {
	// index is the variant's position among all variants: the
	// `variant_index` of the Serializer methods.
	index int
	// ident is the variant's Rust identifier.
	ident string
	attrs variantAttrs
	style style
	// goType is the struct the variant is held in, for an enum held as a Go
	// interface; nil for a data-free enum.
	goType reflect.Type
	// value is the constant of a variant of a data-free enum.
	value uint64
	// fields are the variant's fields.
	fields *fieldSet
	where  string
	// serWith and deWith are the variant's serialize_with and
	// deserialize_with functions, which take the variant's struct.
	serWith func(p unsafe.Pointer, serializer Serializer) (any, error)
	deWith  func(p unsafe.Pointer, deserializer Deserializer) error
}

// effectiveStyleSer and effectiveStyleDe are `effective_style`: a newtype
// variant whose field is skipped is a unit variant.
func (v *variant) effectiveStyleSer() style {
	if v.style == styleNewtype && v.fields.fields[0].attrs.skipSerializing {
		return styleUnit
	}
	return v.style
}

func (v *variant) effectiveStyleDe() style {
	if v.style == styleNewtype && v.fields.fields[0].attrs.skipDeserializing {
		return styleUnit
	}
	return v.style
}

// shapeStyle is the style of a struct or variant registered with shape and
// held in a Go struct with fieldCount fields.
func shapeStyle(shape rs.Shape, fieldCount int) style {
	switch shape {
	case rs.ShapeUnit:
		return styleUnit
	case rs.ShapeNewtype:
		return styleNewtype
	case rs.ShapeTuple:
		if fieldCount == 1 {
			return styleNewtype
		}
		return styleTuple
	}
	return styleStruct
}

// integerValue reads a data-free enum's value as its bits, for lookup.
func integerValue(v reflect.Value) uint64 {
	switch v.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return uint64(v.Int())
	}
	return v.Uint()
}

func unrawIdent(ident string) string { return strings.TrimPrefix(ident, "r#") }
