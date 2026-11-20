// Port of serde_derive 1.0.229 src/ser.rs: the Serialize impl that
// `#[derive(Serialize)]` generates, run over the model in derive.go instead
// of generated as code.

package serde

import (
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// serializeBody is `serialize_body` for a struct.
func (d *derivedStruct) serialize(p unsafe.Pointer, serializer Serializer) (any, error) {
	fs := d.fields
	switch {
	case fs.cont.transparent:
		return serializeTransparent(fs, p, serializer)
	case d.into != nil:
		return d.into(p, serializer)
	}
	switch fs.style {
	case styleStruct:
		return serializeStruct(fs, p, serializer)
	case styleTuple:
		return serializeTupleStruct(fs, p, serializer)
	case styleNewtype:
		return serializer.SerializeNewtypeStruct(fs.cont.name.serialize, fs.fields[0].serValue(p))
	case styleUnit:
		return serializer.SerializeUnitStruct(fs.cont.name.serialize)
	}
	panic(rs.Unreachable(fs.style))
}

// serializeTransparent is `serialize_transparent`.
func serializeTransparent(fs *fieldSet, p unsafe.Pointer, serializer Serializer) (any, error) {
	for _, f := range fs.fields {
		if f.attrs.transparentSer {
			return Serialize(f.serValue(p), serializer)
		}
	}
	panic(rs.Panicf("serde: " + fs.where + ": #[serde(transparent)] requires at least one field that is not skipped"))
}

// serializeTupleStruct is `serialize_tuple_struct`.
func serializeTupleStruct(fs *fieldSet, p unsafe.Pointer, serializer Serializer) (any, error) {
	length := 0
	for _, f := range fs.fields {
		if !f.attrs.skipSerializing && !f.skipped(p) {
			length++
		}
	}
	state, err := serializer.SerializeTupleStruct(fs.cont.name.serialize, length)
	if err != nil {
		return nil, err
	}
	for _, f := range fs.fields {
		if f.attrs.skipSerializing || f.skipped(p) {
			continue
		}
		if err := state.SerializeField(f.serValue(p)); err != nil {
			return nil, err
		}
	}
	return state.End()
}

// serializeStruct is `serialize_struct`.
func serializeStruct(fs *fieldSet, p unsafe.Pointer, serializer Serializer) (any, error) {
	if fs.hasFlattenSer() {
		return serializeStructAsMap(fs, p, serializer)
	}
	return serializeStructAsStruct(fs, p, serializer)
}

// structTagField is `serialize_struct_tag_field`: a struct with
// `#[serde(tag = "...")]` writes its name under the tag.
func structTagField(cont *containerAttrs) (tag string, value string, ok bool) {
	if cont.tag == tagInternal {
		return cont.tagName, cont.name.serialize, true
	}
	return "", "", false
}

// serializeStructAsStruct is `serialize_struct_as_struct`.
func serializeStructAsStruct(fs *fieldSet, p unsafe.Pointer, serializer Serializer) (any, error) {
	tag, tagValue, hasTag := structTagField(fs.cont)
	length := 0
	if hasTag {
		length = 1
	}
	length += serializedFieldCount(fs, p)
	state, err := serializer.SerializeStruct(fs.cont.name.serialize, length)
	if err != nil {
		return nil, err
	}
	if hasTag {
		if err := state.SerializeField(tag, tagValue); err != nil {
			return nil, err
		}
	}
	if err := serializeStructFields(fs, p, state.SerializeField, state.SkipField); err != nil {
		return nil, err
	}
	return state.End()
}

// serializedFieldCount is the `len` of serialize_struct: the fields that are
// not skipped, counting one whose skip_serializing_if holds as none.
func serializedFieldCount(fs *fieldSet, p unsafe.Pointer) int {
	length := 0
	for _, f := range fs.fields {
		if f.attrs.skipSerializing {
			continue
		}
		if !f.skipped(p) {
			length++
		}
	}
	return length
}

// serializeStructFields is `serialize_struct_visitor` for a SerializeStruct
// or a SerializeStructVariant: each field under its name, a field whose
// skip_serializing_if holds reported with skip_field.
func serializeStructFields(fs *fieldSet, p unsafe.Pointer, serializeField func(key string, value any) error, skipField func(key string) error) error {
	for _, f := range fs.fields {
		if f.attrs.skipSerializing {
			continue
		}
		key := f.attrs.name.serialize
		if f.skipped(p) {
			if err := skipField(key); err != nil {
				return err
			}
			continue
		}
		if err := serializeField(key, f.serValue(p)); err != nil {
			return err
		}
	}
	return nil
}

// serializeMapFields is `serialize_struct_visitor` for a SerializeMap: each
// field as an entry, a flattened field through a FlatMapSerializer.
func serializeMapFields(fs *fieldSet, p unsafe.Pointer, state SerSerializeMap) error {
	for _, f := range fs.fields {
		if f.attrs.skipSerializing || f.skipped(p) {
			continue
		}
		if f.attrs.flatten {
			if _, err := Serialize(f.serValue(p), flatMapSerializer{m: state}); err != nil {
				return err
			}
			continue
		}
		if err := state.SerializeEntry(f.attrs.name.serialize, f.serValue(p)); err != nil {
			return err
		}
	}
	return nil
}

// serializeStructAsMap is `serialize_struct_as_map`.
func serializeStructAsMap(fs *fieldSet, p unsafe.Pointer, serializer Serializer) (any, error) {
	state, err := serializer.SerializeMap(rs.None[int]())
	if err != nil {
		return nil, err
	}
	if tag, tagValue, ok := structTagField(fs.cont); ok {
		if err := state.SerializeEntry(tag, tagValue); err != nil {
			return nil, err
		}
	}
	if err := serializeMapFields(fs, p, state); err != nil {
		return nil, err
	}
	return state.End()
}

// serializeVariant is `serialize_variant`.
func (e *derivedEnum) serializeVariant(v *variant, p unsafe.Pointer, serializer Serializer) (any, error) {
	if v.attrs.skipSerializing {
		return nil, SerErrorCustom("the enum variant " + e.typeName + "::" + v.ident + " cannot be serialized")
	}
	switch {
	case e.cont.tag == tagExternal && !v.attrs.untagged:
		return e.serializeExternallyTaggedVariant(v, p, serializer)
	case e.cont.tag == tagInternal && !v.attrs.untagged:
		return e.serializeInternallyTaggedVariant(v, p, serializer)
	case e.cont.tag == tagAdjacent && !v.attrs.untagged:
		return e.serializeAdjacentlyTaggedVariant(v, p, serializer)
	}
	return e.serializeUntaggedVariant(v, p, serializer)
}

// variantWith is `wrap_serialize_variant_with`: the variant's fields handed
// together to its serialize_with function.
func (e *derivedEnum) variantWith(v *variant, p unsafe.Pointer) any {
	return &serializeWithRef{serialize: v.serWith, pointer: p}
}

// serializeExternallyTaggedVariant is `serialize_externally_tagged_variant`.
func (e *derivedEnum) serializeExternallyTaggedVariant(v *variant, p unsafe.Pointer, serializer Serializer) (any, error) {
	typeName := e.cont.name.serialize
	variantName := v.attrs.name.serialize
	index := uint32(v.index)
	if v.serWith != nil {
		return serializer.SerializeNewtypeVariant(typeName, index, variantName, e.variantWith(v, p))
	}
	switch v.effectiveStyleSer() {
	case styleUnit:
		return serializer.SerializeUnitVariant(typeName, index, variantName)
	case styleNewtype:
		return serializer.SerializeNewtypeVariant(typeName, index, variantName, v.fields.fields[0].serValue(p))
	case styleTuple:
		return serializeTupleVariantExternal(v.fields, p, serializer, typeName, index, variantName)
	case styleStruct:
		return serializeStructVariant(v.fields, p, serializer, structVariantExternal, typeName, index, variantName, "")
	}
	panic(rs.Unreachable(v.style))
}

// serializeInternallyTaggedVariant is `serialize_internally_tagged_variant`.
func (e *derivedEnum) serializeInternallyTaggedVariant(v *variant, p unsafe.Pointer, serializer Serializer) (any, error) {
	typeName := e.cont.name.serialize
	variantName := v.attrs.name.serialize
	tag := e.cont.tagName
	if v.serWith != nil {
		return serializeTaggedNewtype(serializer, e.typeName, v.ident, tag, variantName, e.variantWith(v, p))
	}
	switch v.effectiveStyleSer() {
	case styleUnit:
		state, err := serializer.SerializeStruct(typeName, 1)
		if err != nil {
			return nil, err
		}
		if err := state.SerializeField(tag, variantName); err != nil {
			return nil, err
		}
		return state.End()
	case styleNewtype:
		return serializeTaggedNewtype(serializer, e.typeName, v.ident, tag, variantName, v.fields.fields[0].serValue(p))
	case styleStruct:
		return serializeStructVariant(v.fields, p, serializer, structVariantInternal, typeName, 0, variantName, tag)
	}
	panic(rs.Unreachable("serde_derive: internally tagged tuple variant, checked in serde_derive_internals"))
}

// adjacentlyTaggedContent is `__AdjacentlyTagged`: the content of an
// adjacently tagged variant, serialized as the variant would be untagged.
type adjacentlyTaggedContent struct {
	e *derivedEnum
	v *variant
	p unsafe.Pointer
}

func (a *adjacentlyTaggedContent) SerializeSerde(serializer Serializer) (any, error) {
	v, p := a.v, a.p
	if v.serWith != nil {
		return Serialize(a.e.variantWith(v, p), serializer)
	}
	switch v.effectiveStyleSer() {
	case styleTuple:
		return serializeTupleVariantUntagged(v.fields, p, serializer)
	case styleStruct:
		return serializeStructVariant(v.fields, p, serializer, structVariantUntagged, v.attrs.name.serialize, 0, "", "")
	}
	panic(rs.Unreachable(v.style))
}

// serializeAdjacentlyTaggedVariant is `serialize_adjacently_tagged_variant`.
func (e *derivedEnum) serializeAdjacentlyTaggedVariant(v *variant, p unsafe.Pointer, serializer Serializer) (any, error) {
	typeName := e.cont.name.serialize
	tag, content := e.cont.tagName, e.cont.contentName
	variantTag := &adjacentlyTaggedEnumVariant{enumName: typeName, variantIndex: uint32(v.index), variantName: v.attrs.name.serialize}
	if v.serWith == nil {
		switch v.effectiveStyleSer() {
		case styleUnit:
			state, err := serializer.SerializeStruct(typeName, 1)
			if err != nil {
				return nil, err
			}
			if err := state.SerializeField(tag, variantTag); err != nil {
				return nil, err
			}
			return state.End()
		case styleNewtype:
			state, err := serializer.SerializeStruct(typeName, 2)
			if err != nil {
				return nil, err
			}
			if err := state.SerializeField(tag, variantTag); err != nil {
				return nil, err
			}
			if err := state.SerializeField(content, v.fields.fields[0].serValue(p)); err != nil {
				return nil, err
			}
			return state.End()
		}
	}
	state, err := serializer.SerializeStruct(typeName, 2)
	if err != nil {
		return nil, err
	}
	if err := state.SerializeField(tag, variantTag); err != nil {
		return nil, err
	}
	if err := state.SerializeField(content, &adjacentlyTaggedContent{e: e, v: v, p: p}); err != nil {
		return nil, err
	}
	return state.End()
}

// serializeUntaggedVariant is `serialize_untagged_variant`.
func (e *derivedEnum) serializeUntaggedVariant(v *variant, p unsafe.Pointer, serializer Serializer) (any, error) {
	if v.serWith != nil {
		return Serialize(e.variantWith(v, p), serializer)
	}
	switch v.effectiveStyleSer() {
	case styleUnit:
		return serializer.SerializeUnit()
	case styleNewtype:
		return Serialize(v.fields.fields[0].serValue(p), serializer)
	case styleTuple:
		return serializeTupleVariantUntagged(v.fields, p, serializer)
	case styleStruct:
		return serializeStructVariant(v.fields, p, serializer, structVariantUntagged, e.cont.name.serialize, 0, "", "")
	}
	panic(rs.Unreachable(v.style))
}

// serializedTupleLength is the `len` of serialize_tuple_variant.
func serializedTupleLength(fs *fieldSet, p unsafe.Pointer) int {
	length := 0
	for _, f := range fs.fields {
		if !f.attrs.skipSerializing && !f.skipped(p) {
			length++
		}
	}
	return length
}

// serializeTupleElements is `serialize_tuple_struct_visitor`.
func serializeTupleElements(fs *fieldSet, p unsafe.Pointer, serializeElement func(value any) error) error {
	for _, f := range fs.fields {
		if f.attrs.skipSerializing || f.skipped(p) {
			continue
		}
		if err := serializeElement(f.serValue(p)); err != nil {
			return err
		}
	}
	return nil
}

// serializeTupleVariantExternal is `serialize_tuple_variant` with
// `TupleVariant::ExternallyTagged`.
func serializeTupleVariantExternal(fs *fieldSet, p unsafe.Pointer, serializer Serializer, typeName string, index uint32, variantName string) (any, error) {
	state, err := serializer.SerializeTupleVariant(typeName, index, variantName, serializedTupleLength(fs, p))
	if err != nil {
		return nil, err
	}
	if err := serializeTupleElements(fs, p, state.SerializeField); err != nil {
		return nil, err
	}
	return state.End()
}

// serializeTupleVariantUntagged is `serialize_tuple_variant` with
// `TupleVariant::Untagged`.
func serializeTupleVariantUntagged(fs *fieldSet, p unsafe.Pointer, serializer Serializer) (any, error) {
	state, err := serializer.SerializeTuple(serializedTupleLength(fs, p))
	if err != nil {
		return nil, err
	}
	if err := serializeTupleElements(fs, p, state.SerializeElement); err != nil {
		return nil, err
	}
	return state.End()
}

// structVariantContext is `StructVariant`.
type structVariantContext uint8

const (
	structVariantExternal structVariantContext = iota
	structVariantInternal
	structVariantUntagged
)

// serializeStructVariant is `serialize_struct_variant`. name is the name
// given to the Serializer; index and variantName are those of an
// externally tagged variant, and variantName and tag those of an internally
// tagged one.
func serializeStructVariant(fs *fieldSet, p unsafe.Pointer, serializer Serializer, context structVariantContext, name string, index uint32, variantName string, tag string) (any, error) {
	for _, f := range fs.fields {
		if f.attrs.flatten {
			return serializeStructVariantWithFlatten(fs, p, serializer, context, name, index, variantName, tag)
		}
	}
	length := serializedFieldCount(fs, p)
	switch context {
	case structVariantExternal:
		state, err := serializer.SerializeStructVariant(name, index, variantName, length)
		if err != nil {
			return nil, err
		}
		if err := serializeStructFields(fs, p, state.SerializeField, state.SkipField); err != nil {
			return nil, err
		}
		return state.End()
	case structVariantInternal:
		state, err := serializer.SerializeStruct(name, length+1)
		if err != nil {
			return nil, err
		}
		if err := state.SerializeField(tag, variantName); err != nil {
			return nil, err
		}
		if err := serializeStructFields(fs, p, state.SerializeField, state.SkipField); err != nil {
			return nil, err
		}
		return state.End()
	}
	state, err := serializer.SerializeStruct(name, length)
	if err != nil {
		return nil, err
	}
	if err := serializeStructFields(fs, p, state.SerializeField, state.SkipField); err != nil {
		return nil, err
	}
	return state.End()
}

// enumFlatten is `__EnumFlatten`: the fields of an externally tagged struct
// variant with a flattened field, serialized as a map.
type enumFlatten struct {
	fs *fieldSet
	p  unsafe.Pointer
}

func (f *enumFlatten) SerializeSerde(serializer Serializer) (any, error) {
	state, err := serializer.SerializeMap(rs.None[int]())
	if err != nil {
		return nil, err
	}
	if err := serializeMapFields(f.fs, f.p, state); err != nil {
		return nil, err
	}
	return state.End()
}

// serializeStructVariantWithFlatten is
// `serialize_struct_variant_with_flatten`.
func serializeStructVariantWithFlatten(fs *fieldSet, p unsafe.Pointer, serializer Serializer, context structVariantContext, name string, index uint32, variantName string, tag string) (any, error) {
	switch context {
	case structVariantExternal:
		return serializer.SerializeNewtypeVariant(name, index, variantName, &enumFlatten{fs: fs, p: p})
	case structVariantInternal:
		state, err := serializer.SerializeMap(rs.None[int]())
		if err != nil {
			return nil, err
		}
		if err := state.SerializeEntry(tag, variantName); err != nil {
			return nil, err
		}
		if err := serializeMapFields(fs, p, state); err != nil {
			return nil, err
		}
		return state.End()
	}
	state, err := serializer.SerializeMap(rs.None[int]())
	if err != nil {
		return nil, err
	}
	if err := serializeMapFields(fs, p, state); err != nil {
		return nil, err
	}
	return state.End()
}
