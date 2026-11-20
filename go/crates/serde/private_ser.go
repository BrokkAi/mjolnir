// Port of serde 1.0.229 src/private/ser.rs

package serde

import (
	"math"
	"slices"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// serializeTaggedNewtype is `serialize_tagged_newtype`: it serializes the
// one value of a newtype variant of an internally tagged enum, with the tag
// added to it. Not public API.
func serializeTaggedNewtype(serializer Serializer, typeIdent string, variantIdent string, tag string, variantName string, value any) (any, error) {
	return Serialize(value, &taggedSerializer{
		typeIdent:    typeIdent,
		variantIdent: variantIdent,
		tag:          tag,
		variantName:  variantName,
		delegate:     serializer,
	})
}

type taggedSerializer struct {
	typeIdent    string
	variantIdent string
	tag          string
	variantName  string
	delegate     Serializer
}

// unsupported is the Rust enum `Unsupported`: what a tagged or flattened
// value cannot be.
type unsupported string

const (
	unsupportedBoolean     unsupported = "a boolean"
	unsupportedInteger     unsupported = "an integer"
	unsupportedFloat       unsupported = "a float"
	unsupportedChar        unsupported = "a char"
	unsupportedString      unsupported = "a string"
	unsupportedByteArray   unsupported = "a byte array"
	unsupportedOptional    unsupported = "an optional"
	unsupportedSequence    unsupported = "a sequence"
	unsupportedTuple       unsupported = "a tuple"
	unsupportedTupleStruct unsupported = "a tuple struct"
)

// unitValue serializes as `()`.
type unitValue struct{}

func (unitValue) SerializeSerde(serializer Serializer) (any, error) {
	return serializer.SerializeUnit()
}

func (s *taggedSerializer) badType(what unsupported) error {
	return SerErrorCustom("cannot serialize tagged newtype variant " + s.typeIdent + "::" + s.variantIdent + " containing " + string(what))
}

func (s *taggedSerializer) SerializeBool(bool) (any, error) {
	return nil, s.badType(unsupportedBoolean)
}
func (s *taggedSerializer) SerializeI8(int8) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeI16(int16) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeI32(int32) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeI64(int64) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeU8(uint8) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeU16(uint16) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeU32(uint32) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeU64(uint64) (any, error) {
	return nil, s.badType(unsupportedInteger)
}
func (s *taggedSerializer) SerializeF32(float32) (any, error) {
	return nil, s.badType(unsupportedFloat)
}
func (s *taggedSerializer) SerializeF64(float64) (any, error) {
	return nil, s.badType(unsupportedFloat)
}
func (s *taggedSerializer) SerializeChar(rune) (any, error) {
	return nil, s.badType(unsupportedChar)
}
func (s *taggedSerializer) SerializeStr(string) (any, error) {
	return nil, s.badType(unsupportedString)
}
func (s *taggedSerializer) SerializeBytes([]byte) (any, error) {
	return nil, s.badType(unsupportedByteArray)
}
func (s *taggedSerializer) SerializeNone() (any, error) {
	return nil, s.badType(unsupportedOptional)
}
func (s *taggedSerializer) SerializeSome(any) (any, error) {
	return nil, s.badType(unsupportedOptional)
}

func (s *taggedSerializer) SerializeUnit() (any, error) {
	m, err := s.delegate.SerializeMap(rs.Some(1))
	if err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(s.tag, s.variantName); err != nil {
		return nil, err
	}
	return m.End()
}

func (s *taggedSerializer) SerializeUnitStruct(string) (any, error) {
	return s.SerializeUnit()
}

func (s *taggedSerializer) SerializeUnitVariant(_ string, _ uint32, innerVariant string) (any, error) {
	m, err := s.delegate.SerializeMap(rs.Some(2))
	if err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(s.tag, s.variantName); err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(innerVariant, unitValue{}); err != nil {
		return nil, err
	}
	return m.End()
}

func (s *taggedSerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return Serialize(value, s)
}

func (s *taggedSerializer) SerializeNewtypeVariant(_ string, _ uint32, innerVariant string, innerValue any) (any, error) {
	m, err := s.delegate.SerializeMap(rs.Some(2))
	if err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(s.tag, s.variantName); err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(innerVariant, innerValue); err != nil {
		return nil, err
	}
	return m.End()
}

func (s *taggedSerializer) SerializeSeq(rs.Option[int]) (SerSerializeSeq, error) {
	return nil, s.badType(unsupportedSequence)
}

func (s *taggedSerializer) SerializeTuple(int) (SerSerializeTuple, error) {
	return nil, s.badType(unsupportedTuple)
}

func (s *taggedSerializer) SerializeTupleStruct(string, int) (SerSerializeTupleStruct, error) {
	return nil, s.badType(unsupportedTupleStruct)
}

func (s *taggedSerializer) SerializeTupleVariant(_ string, _ uint32, innerVariant string, length int) (SerSerializeTupleVariant, error) {
	m, err := s.delegate.SerializeMap(rs.Some(2))
	if err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(s.tag, s.variantName); err != nil {
		return nil, err
	}
	if err := m.SerializeKey(innerVariant); err != nil {
		return nil, err
	}
	return &serializeTupleVariantAsMapValue{m: m, name: innerVariant, fields: make([]serContent, 0, length)}, nil
}

func (s *taggedSerializer) SerializeMap(length rs.Option[int]) (SerSerializeMap, error) {
	m, err := s.delegate.SerializeMap(rs.OptionMap(length, func(n int) int { return n + 1 }))
	if err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(s.tag, s.variantName); err != nil {
		return nil, err
	}
	return m, nil
}

func (s *taggedSerializer) SerializeStruct(name string, length int) (SerSerializeStruct, error) {
	state, err := s.delegate.SerializeStruct(name, length+1)
	if err != nil {
		return nil, err
	}
	if err := state.SerializeField(s.tag, s.variantName); err != nil {
		return nil, err
	}
	return state, nil
}

func (s *taggedSerializer) SerializeStructVariant(_ string, _ uint32, innerVariant string, length int) (SerSerializeStructVariant, error) {
	m, err := s.delegate.SerializeMap(rs.Some(2))
	if err != nil {
		return nil, err
	}
	if err := m.SerializeEntry(s.tag, s.variantName); err != nil {
		return nil, err
	}
	if err := m.SerializeKey(innerVariant); err != nil {
		return nil, err
	}
	return &serializeStructVariantAsMapValue{m: m, name: innerVariant, fields: make([]serContentField, 0, length)}, nil
}

func (s *taggedSerializer) CollectStr(value string) (any, error) {
	return SerializerDefaultCollectStr(s, value)
}

func (s *taggedSerializer) IsHumanReadable() bool { return SerializerDefaultIsHumanReadable() }

// serializeTupleVariantAsMapValue is `SerializeTupleVariantAsMapValue`.
type serializeTupleVariantAsMapValue struct {
	m      SerSerializeMap
	name   string
	fields []serContent
}

func (s *serializeTupleVariantAsMapValue) SerializeField(value any) error {
	field, err := serializeToContent(value)
	if err != nil {
		return err
	}
	s.fields = append(s.fields, field)
	return nil
}

func (s *serializeTupleVariantAsMapValue) End() (any, error) {
	if err := s.m.SerializeValue(&serContent{kind: serContentTupleStruct, name: s.name, elements: s.fields}); err != nil {
		return nil, err
	}
	return s.m.End()
}

// serializeStructVariantAsMapValue is `SerializeStructVariantAsMapValue`.
type serializeStructVariantAsMapValue struct {
	m      SerSerializeMap
	name   string
	fields []serContentField
}

func (s *serializeStructVariantAsMapValue) SerializeField(key string, value any) error {
	field, err := serializeToContent(value)
	if err != nil {
		return err
	}
	s.fields = append(s.fields, serContentField{key: key, value: field})
	return nil
}

func (s *serializeStructVariantAsMapValue) SkipField(string) error { return nil }

func (s *serializeStructVariantAsMapValue) End() (any, error) {
	if err := s.m.SerializeValue(&serContent{kind: serContentStruct, name: s.name, fields: s.fields}); err != nil {
		return nil, err
	}
	return s.m.End()
}

type serContentKind uint8

const (
	serContentInvalid serContentKind = iota
	serContentBool
	serContentU8
	serContentU16
	serContentU32
	serContentU64
	serContentI8
	serContentI16
	serContentI32
	serContentI64
	serContentF32
	serContentF64
	serContentChar
	serContentString
	serContentBytes
	serContentNone
	serContentSome
	serContentUnit
	serContentUnitStruct
	serContentUnitVariant
	serContentNewtypeStruct
	serContentNewtypeVariant
	serContentSeq
	serContentTuple
	serContentTupleStruct
	serContentTupleVariant
	serContentMap
	serContentStruct
	serContentStructVariant
)

// serContent is the `Content` of private/ser.rs: a serialized value held in
// memory, which serializes as the value did.
type serContent struct {
	kind serContentKind
	// num holds a Bool, an integer, the bits of a float or a Char, as in the
	// content of private_de.go.
	num   uint64
	str   string
	bytes []byte
	// name, variantIndex and variant are the arguments of the Serializer
	// method the content came from.
	name         string
	variantIndex uint32
	variant      string
	// elements holds a Seq, Tuple, TupleStruct or TupleVariant, or the one
	// boxed content of a Some, NewtypeStruct or NewtypeVariant.
	elements []serContent
	entries  []serContentEntry
	fields   []serContentField
}

type serContentEntry struct{ key, value serContent }

type serContentField struct {
	key   string
	value serContent
}

func (c *serContent) SerializeSerde(serializer Serializer) (any, error) {
	switch c.kind {
	case serContentBool:
		return serializer.SerializeBool(c.num != 0)
	case serContentU8:
		return serializer.SerializeU8(uint8(c.num))
	case serContentU16:
		return serializer.SerializeU16(uint16(c.num))
	case serContentU32:
		return serializer.SerializeU32(uint32(c.num))
	case serContentU64:
		return serializer.SerializeU64(c.num)
	case serContentI8:
		return serializer.SerializeI8(int8(c.num))
	case serContentI16:
		return serializer.SerializeI16(int16(c.num))
	case serContentI32:
		return serializer.SerializeI32(int32(c.num))
	case serContentI64:
		return serializer.SerializeI64(int64(c.num))
	case serContentF32:
		return serializer.SerializeF32(math.Float32frombits(uint32(c.num)))
	case serContentF64:
		return serializer.SerializeF64(math.Float64frombits(c.num))
	case serContentChar:
		return serializer.SerializeChar(rune(c.num))
	case serContentString:
		return serializer.SerializeStr(c.str)
	case serContentBytes:
		return serializer.SerializeBytes(c.bytes)
	case serContentNone:
		return serializer.SerializeNone()
	case serContentSome:
		return serializer.SerializeSome(&c.elements[0])
	case serContentUnit:
		return serializer.SerializeUnit()
	case serContentUnitStruct:
		return serializer.SerializeUnitStruct(c.name)
	case serContentUnitVariant:
		return serializer.SerializeUnitVariant(c.name, c.variantIndex, c.variant)
	case serContentNewtypeStruct:
		return serializer.SerializeNewtypeStruct(c.name, &c.elements[0])
	case serContentNewtypeVariant:
		return serializer.SerializeNewtypeVariant(c.name, c.variantIndex, c.variant, &c.elements[0])
	case serContentSeq:
		seq, err := serializer.SerializeSeq(rs.Some(len(c.elements)))
		if err != nil {
			return nil, err
		}
		for index := range c.elements {
			if err := seq.SerializeElement(&c.elements[index]); err != nil {
				return nil, err
			}
		}
		return seq.End()
	case serContentTuple:
		tuple, err := serializer.SerializeTuple(len(c.elements))
		if err != nil {
			return nil, err
		}
		for index := range c.elements {
			if err := tuple.SerializeElement(&c.elements[index]); err != nil {
				return nil, err
			}
		}
		return tuple.End()
	case serContentTupleStruct:
		ts, err := serializer.SerializeTupleStruct(c.name, len(c.elements))
		if err != nil {
			return nil, err
		}
		for index := range c.elements {
			if err := ts.SerializeField(&c.elements[index]); err != nil {
				return nil, err
			}
		}
		return ts.End()
	case serContentTupleVariant:
		tv, err := serializer.SerializeTupleVariant(c.name, c.variantIndex, c.variant, len(c.elements))
		if err != nil {
			return nil, err
		}
		for index := range c.elements {
			if err := tv.SerializeField(&c.elements[index]); err != nil {
				return nil, err
			}
		}
		return tv.End()
	case serContentMap:
		m, err := serializer.SerializeMap(rs.Some(len(c.entries)))
		if err != nil {
			return nil, err
		}
		for index := range c.entries {
			if err := m.SerializeEntry(&c.entries[index].key, &c.entries[index].value); err != nil {
				return nil, err
			}
		}
		return m.End()
	case serContentStruct:
		s, err := serializer.SerializeStruct(c.name, len(c.fields))
		if err != nil {
			return nil, err
		}
		for index := range c.fields {
			if err := s.SerializeField(c.fields[index].key, &c.fields[index].value); err != nil {
				return nil, err
			}
		}
		return s.End()
	case serContentStructVariant:
		sv, err := serializer.SerializeStructVariant(c.name, c.variantIndex, c.variant, len(c.fields))
		if err != nil {
			return nil, err
		}
		for index := range c.fields {
			if err := sv.SerializeField(c.fields[index].key, &c.fields[index].value); err != nil {
				return nil, err
			}
		}
		return sv.End()
	}
	panic(rs.Unreachable("serde: serialized content of no kind"))
}

// serializeToContent is `value.serialize(ContentSerializer::new())`.
func serializeToContent(value any) (serContent, error) {
	built, err := Serialize(value, contentSerializer{})
	if err != nil {
		return serContent{}, err
	}
	return *built.(*serContent), nil
}

// contentSerializer is `ContentSerializer`: it builds the serContent of the
// value it is given. Its `Ok` is a *serContent.
type contentSerializer struct{}

func scalarContent(kind serContentKind, num uint64) (any, error) {
	return &serContent{kind: kind, num: num}, nil
}

func (contentSerializer) SerializeBool(v bool) (any, error) {
	if v {
		return scalarContent(serContentBool, 1)
	}
	return scalarContent(serContentBool, 0)
}
func (contentSerializer) SerializeI8(v int8) (any, error) {
	return scalarContent(serContentI8, uint64(int64(v)))
}
func (contentSerializer) SerializeI16(v int16) (any, error) {
	return scalarContent(serContentI16, uint64(int64(v)))
}
func (contentSerializer) SerializeI32(v int32) (any, error) {
	return scalarContent(serContentI32, uint64(int64(v)))
}
func (contentSerializer) SerializeI64(v int64) (any, error) {
	return scalarContent(serContentI64, uint64(v))
}
func (contentSerializer) SerializeU8(v uint8) (any, error) {
	return scalarContent(serContentU8, uint64(v))
}
func (contentSerializer) SerializeU16(v uint16) (any, error) {
	return scalarContent(serContentU16, uint64(v))
}
func (contentSerializer) SerializeU32(v uint32) (any, error) {
	return scalarContent(serContentU32, uint64(v))
}
func (contentSerializer) SerializeU64(v uint64) (any, error) {
	return scalarContent(serContentU64, v)
}
func (contentSerializer) SerializeF32(v float32) (any, error) {
	return scalarContent(serContentF32, uint64(math.Float32bits(v)))
}
func (contentSerializer) SerializeF64(v float64) (any, error) {
	return scalarContent(serContentF64, math.Float64bits(v))
}
func (contentSerializer) SerializeChar(v rune) (any, error) {
	return scalarContent(serContentChar, uint64(v))
}
func (contentSerializer) SerializeStr(v string) (any, error) {
	return &serContent{kind: serContentString, str: v}, nil
}
func (contentSerializer) SerializeBytes(v []byte) (any, error) {
	return &serContent{kind: serContentBytes, bytes: slices.Clone(v)}, nil
}
func (contentSerializer) SerializeNone() (any, error) { return scalarContent(serContentNone, 0) }

func (s contentSerializer) boxed(value any) ([]serContent, error) {
	inner, err := serializeToContent(value)
	if err != nil {
		return nil, err
	}
	return []serContent{inner}, nil
}

func (s contentSerializer) SerializeSome(value any) (any, error) {
	boxed, err := s.boxed(value)
	if err != nil {
		return nil, err
	}
	return &serContent{kind: serContentSome, elements: boxed}, nil
}

func (contentSerializer) SerializeUnit() (any, error) { return scalarContent(serContentUnit, 0) }

func (contentSerializer) SerializeUnitStruct(name string) (any, error) {
	return &serContent{kind: serContentUnitStruct, name: name}, nil
}

func (contentSerializer) SerializeUnitVariant(name string, variantIndex uint32, variant string) (any, error) {
	return &serContent{kind: serContentUnitVariant, name: name, variantIndex: variantIndex, variant: variant}, nil
}

func (s contentSerializer) SerializeNewtypeStruct(name string, value any) (any, error) {
	boxed, err := s.boxed(value)
	if err != nil {
		return nil, err
	}
	return &serContent{kind: serContentNewtypeStruct, name: name, elements: boxed}, nil
}

func (s contentSerializer) SerializeNewtypeVariant(name string, variantIndex uint32, variant string, value any) (any, error) {
	boxed, err := s.boxed(value)
	if err != nil {
		return nil, err
	}
	return &serContent{kind: serContentNewtypeVariant, name: name, variantIndex: variantIndex, variant: variant, elements: boxed}, nil
}

func (contentSerializer) SerializeSeq(length rs.Option[int]) (SerSerializeSeq, error) {
	return &contentCompound{content: serContent{kind: serContentSeq, elements: make([]serContent, 0, length.UnwrapOr(0))}}, nil
}

func (contentSerializer) SerializeTuple(length int) (SerSerializeTuple, error) {
	return &contentCompound{content: serContent{kind: serContentTuple, elements: make([]serContent, 0, length)}}, nil
}

func (contentSerializer) SerializeTupleStruct(name string, length int) (SerSerializeTupleStruct, error) {
	return &contentCompoundFields{contentCompound{content: serContent{kind: serContentTupleStruct, name: name, elements: make([]serContent, 0, length)}}}, nil
}

func (contentSerializer) SerializeTupleVariant(name string, variantIndex uint32, variant string, length int) (SerSerializeTupleVariant, error) {
	return &contentCompoundFields{contentCompound{content: serContent{kind: serContentTupleVariant, name: name, variantIndex: variantIndex, variant: variant, elements: make([]serContent, 0, length)}}}, nil
}

func (contentSerializer) SerializeMap(length rs.Option[int]) (SerSerializeMap, error) {
	return &contentCompoundMap{content: serContent{kind: serContentMap, entries: make([]serContentEntry, 0, length.UnwrapOr(0))}}, nil
}

func (contentSerializer) SerializeStruct(name string, length int) (SerSerializeStruct, error) {
	return &contentCompoundStruct{content: serContent{kind: serContentStruct, name: name, fields: make([]serContentField, 0, length)}}, nil
}

func (contentSerializer) SerializeStructVariant(name string, variantIndex uint32, variant string, length int) (SerSerializeStructVariant, error) {
	return &contentCompoundStruct{content: serContent{kind: serContentStructVariant, name: name, variantIndex: variantIndex, variant: variant, fields: make([]serContentField, 0, length)}}, nil
}

func (s contentSerializer) CollectStr(value string) (any, error) {
	return SerializerDefaultCollectStr(s, value)
}

func (contentSerializer) IsHumanReadable() bool { return SerializerDefaultIsHumanReadable() }

// contentCompound collects the elements of a Seq or a Tuple.
type contentCompound struct{ content serContent }

func (c *contentCompound) SerializeElement(value any) error {
	element, err := serializeToContent(value)
	if err != nil {
		return err
	}
	c.content.elements = append(c.content.elements, element)
	return nil
}

func (c *contentCompound) End() (any, error) { return &c.content, nil }

// contentCompoundFields collects the fields of a TupleStruct or a
// TupleVariant.
type contentCompoundFields struct{ contentCompound }

func (c *contentCompoundFields) SerializeField(value any) error { return c.SerializeElement(value) }

// contentCompoundMap collects the entries of a Map.
type contentCompoundMap struct {
	content serContent
	key     serContent
	hasKey  bool
}

func (c *contentCompoundMap) SerializeKey(key any) error {
	built, err := serializeToContent(key)
	if err != nil {
		return err
	}
	c.key, c.hasKey = built, true
	return nil
}

func (c *contentCompoundMap) SerializeValue(value any) error {
	if !c.hasKey {
		panic(rs.Panicf("serialize_value called before serialize_key"))
	}
	c.hasKey = false
	built, err := serializeToContent(value)
	if err != nil {
		return err
	}
	c.content.entries = append(c.content.entries, serContentEntry{key: c.key, value: built})
	return nil
}

func (c *contentCompoundMap) SerializeEntry(key any, value any) error {
	builtKey, err := serializeToContent(key)
	if err != nil {
		return err
	}
	builtValue, err := serializeToContent(value)
	if err != nil {
		return err
	}
	c.content.entries = append(c.content.entries, serContentEntry{key: builtKey, value: builtValue})
	return nil
}

func (c *contentCompoundMap) End() (any, error) { return &c.content, nil }

// contentCompoundStruct collects the fields of a Struct or a StructVariant.
type contentCompoundStruct struct{ content serContent }

func (c *contentCompoundStruct) SerializeField(key string, value any) error {
	built, err := serializeToContent(value)
	if err != nil {
		return err
	}
	c.content.fields = append(c.content.fields, serContentField{key: key, value: built})
	return nil
}

func (c *contentCompoundStruct) SkipField(string) error { return nil }

func (c *contentCompoundStruct) End() (any, error) { return &c.content, nil }

// flatMapSerializer is `FlatMapSerializer(&mut map)`: what a flattened field
// is serialized to. It writes the entries of the field into the map of the
// struct that holds it. Its `Ok` is `()`.
type flatMapSerializer struct{ m SerSerializeMap }

func flatMapBadType(what unsupported) error {
	return SerErrorCustom("can only flatten structs and maps (got " + string(what) + ")")
}

func (flatMapSerializer) SerializeBool(bool) (any, error) {
	return nil, flatMapBadType(unsupportedBoolean)
}
func (flatMapSerializer) SerializeI8(int8) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeI16(int16) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeI32(int32) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeI64(int64) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeU8(uint8) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeU16(uint16) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeU32(uint32) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeU64(uint64) (any, error) {
	return nil, flatMapBadType(unsupportedInteger)
}
func (flatMapSerializer) SerializeF32(float32) (any, error) {
	return nil, flatMapBadType(unsupportedFloat)
}
func (flatMapSerializer) SerializeF64(float64) (any, error) {
	return nil, flatMapBadType(unsupportedFloat)
}
func (flatMapSerializer) SerializeChar(rune) (any, error) {
	return nil, flatMapBadType(unsupportedChar)
}
func (flatMapSerializer) SerializeStr(string) (any, error) {
	return nil, flatMapBadType(unsupportedString)
}
func (flatMapSerializer) SerializeBytes([]byte) (any, error) {
	return nil, flatMapBadType(unsupportedByteArray)
}

func (flatMapSerializer) SerializeNone() (any, error) { return nil, nil }

func (s flatMapSerializer) SerializeSome(value any) (any, error) { return Serialize(value, s) }

func (flatMapSerializer) SerializeUnit() (any, error) { return nil, nil }

func (flatMapSerializer) SerializeUnitStruct(string) (any, error) { return nil, nil }

func (s flatMapSerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return nil, s.m.SerializeEntry(variant, unitValue{})
}

func (s flatMapSerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return Serialize(value, s)
}

func (s flatMapSerializer) SerializeNewtypeVariant(_ string, _ uint32, variant string, value any) (any, error) {
	return nil, s.m.SerializeEntry(variant, value)
}

func (flatMapSerializer) SerializeSeq(rs.Option[int]) (SerSerializeSeq, error) {
	return nil, flatMapBadType(unsupportedSequence)
}

func (flatMapSerializer) SerializeTuple(int) (SerSerializeTuple, error) {
	return nil, flatMapBadType(unsupportedTuple)
}

func (flatMapSerializer) SerializeTupleStruct(string, int) (SerSerializeTupleStruct, error) {
	return nil, flatMapBadType(unsupportedTupleStruct)
}

func (s flatMapSerializer) SerializeTupleVariant(_ string, _ uint32, variant string, _ int) (SerSerializeTupleVariant, error) {
	if err := s.m.SerializeKey(variant); err != nil {
		return nil, err
	}
	return &flatMapSerializeTupleVariantAsMapValue{m: s.m}, nil
}

func (s flatMapSerializer) SerializeMap(rs.Option[int]) (SerSerializeMap, error) {
	return flatMapSerializeMap{m: s.m}, nil
}

func (s flatMapSerializer) SerializeStruct(string, int) (SerSerializeStruct, error) {
	return flatMapSerializeStruct{m: s.m}, nil
}

func (s flatMapSerializer) SerializeStructVariant(_ string, _ uint32, innerVariant string, _ int) (SerSerializeStructVariant, error) {
	if err := s.m.SerializeKey(innerVariant); err != nil {
		return nil, err
	}
	return &flatMapSerializeStructVariantAsMapValue{m: s.m, name: innerVariant}, nil
}

func (s flatMapSerializer) CollectStr(value string) (any, error) {
	return SerializerDefaultCollectStr(s, value)
}

func (flatMapSerializer) IsHumanReadable() bool { return SerializerDefaultIsHumanReadable() }

// flatMapSerializeMap is `FlatMapSerializeMap`.
type flatMapSerializeMap struct{ m SerSerializeMap }

func (f flatMapSerializeMap) SerializeKey(key any) error     { return f.m.SerializeKey(key) }
func (f flatMapSerializeMap) SerializeValue(value any) error { return f.m.SerializeValue(value) }
func (f flatMapSerializeMap) SerializeEntry(key any, value any) error {
	return f.m.SerializeEntry(key, value)
}
func (f flatMapSerializeMap) End() (any, error) { return nil, nil }

// flatMapSerializeStruct is `FlatMapSerializeStruct`.
type flatMapSerializeStruct struct{ m SerSerializeMap }

func (f flatMapSerializeStruct) SerializeField(key string, value any) error {
	return f.m.SerializeEntry(key, value)
}
func (f flatMapSerializeStruct) SkipField(string) error { return nil }
func (f flatMapSerializeStruct) End() (any, error)      { return nil, nil }

// flatMapSerializeTupleVariantAsMapValue is
// `FlatMapSerializeTupleVariantAsMapValue`.
type flatMapSerializeTupleVariantAsMapValue struct {
	m      SerSerializeMap
	fields []serContent
}

func (f *flatMapSerializeTupleVariantAsMapValue) SerializeField(value any) error {
	field, err := serializeToContent(value)
	if err != nil {
		return err
	}
	f.fields = append(f.fields, field)
	return nil
}

func (f *flatMapSerializeTupleVariantAsMapValue) End() (any, error) {
	return nil, f.m.SerializeValue(&serContent{kind: serContentSeq, elements: f.fields})
}

// flatMapSerializeStructVariantAsMapValue is
// `FlatMapSerializeStructVariantAsMapValue`.
type flatMapSerializeStructVariantAsMapValue struct {
	m      SerSerializeMap
	name   string
	fields []serContentField
}

func (f *flatMapSerializeStructVariantAsMapValue) SerializeField(key string, value any) error {
	field, err := serializeToContent(value)
	if err != nil {
		return err
	}
	f.fields = append(f.fields, serContentField{key: key, value: field})
	return nil
}

func (f *flatMapSerializeStructVariantAsMapValue) SkipField(string) error { return nil }

func (f *flatMapSerializeStructVariantAsMapValue) End() (any, error) {
	return nil, f.m.SerializeValue(&serContent{kind: serContentStruct, name: f.name, fields: f.fields})
}

// adjacentlyTaggedEnumVariant is `AdjacentlyTaggedEnumVariant`: the tag of
// an adjacently tagged enum, which serializes as a unit variant.
type adjacentlyTaggedEnumVariant struct {
	enumName     string
	variantIndex uint32
	variantName  string
}

func (v *adjacentlyTaggedEnumVariant) SerializeSerde(serializer Serializer) (any, error) {
	return serializer.SerializeUnitVariant(v.enumName, v.variantIndex, v.variantName)
}

var (
	_ Serializer = (*taggedSerializer)(nil)
	_ Serializer = contentSerializer{}
	_ Serializer = flatMapSerializer{}
)
