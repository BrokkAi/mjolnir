// Port of serde_json 1.0.150 src/value/ser.rs, src/value/de.rs and the
// serde impls of src/map.rs

package serdejson

import (
	"math"
	"reflect"
	"strconv"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

var _ = serde.RegisterSerialize(func(value Value, serializer serde.Serializer) (any, error) {
	return serializeValue(value, serializer)
})

var _ = serde.RegisterDeserialize(func(deserializer serde.Deserializer) (Value, error) {
	return deserializeValue(deserializer)
})

var _ = serde.RegisterSerialize(func(value Map, serializer serde.Serializer) (any, error) {
	return serializeMap(value, serializer)
})

var _ = serde.RegisterDeserialize(func(deserializer serde.Deserializer) (Map, error) {
	var out Map
	_, err := deserializer.DeserializeMap(&mapVisitor{out: &out})
	return out, err
})

// serializeValue is `impl Serialize for Value`. A value the jsonvalue
// representation does not hold (a struct, a Go map) is serialized as its
// own type is.
func serializeValue(value Value, serializer serde.Serializer) (any, error) {
	switch v := value.(type) {
	case nil:
		return serializer.SerializeUnit()
	case bool:
		return serializer.SerializeBool(v)
	case string:
		return serializer.SerializeStr(v)
	case []any:
		seq, err := serializer.SerializeSeq(rs.Some(len(v)))
		if err != nil {
			return nil, err
		}
		for _, element := range v {
			if err := seq.SerializeElement(valueRef{element}); err != nil {
				return nil, err
			}
		}
		return seq.End()
	case Map:
		return serializeMap(v, serializer)
	}
	if n, ok := numberOf(value); ok {
		return n.SerializeSerde(serializer)
	}
	return serde.Serialize(value, serializer)
}

// valueRef hands a Value to a Serializer: it serializes as the Value, even
// when the Value is nil.
type valueRef struct{ v Value }

func (r valueRef) SerializeSerde(serializer serde.Serializer) (any, error) {
	return serializeValue(r.v, serializer)
}

// serializeMap is `impl Serialize for Map<String, Value>`.
func serializeMap(m Map, serializer serde.Serializer) (any, error) {
	m = mapMerged(m)
	state, err := serializer.SerializeMap(rs.Some(len(m)))
	if err != nil {
		return nil, err
	}
	for _, member := range m {
		if err := state.SerializeEntry(member.Key, valueRef{member.Value}); err != nil {
			return nil, err
		}
	}
	return state.End()
}

// mapMerged returns m with each key once, at its first position with its
// last value, as the wire reads an Object built with a repeated key.
func mapMerged(m Map) Map {
	if len(m) < 2 {
		return m
	}
	seen := make(map[string]int, len(m))
	repeats := false
	for i, member := range m {
		if _, ok := seen[member.Key]; ok {
			repeats = true
			break
		}
		seen[member.Key] = i
	}
	if !repeats {
		return m
	}
	index := make(map[string]int, len(m))
	out := make(Map, 0, len(m))
	for _, member := range m {
		if i, ok := index[member.Key]; ok {
			out[i].Value = member.Value
			continue
		}
		index[member.Key] = len(out)
		out = append(out, member)
	}
	return out
}

// deserializeValue is `impl Deserialize for Value`.
func deserializeValue(deserializer serde.Deserializer) (Value, error) {
	var out Value
	_, err := deserializer.DeserializeAny(&valueVisitor{out: &out})
	return out, err
}

// valueVisitor is `ValueVisitor`.
type valueVisitor struct{ out *Value }

func (v *valueVisitor) Expecting() string { return "any valid JSON value" }

func (v *valueVisitor) VisitBool(value bool) (any, error) {
	*v.out = value
	return nil, nil
}

func (v *valueVisitor) VisitI64(value int64) (any, error) {
	*v.out = value
	return nil, nil
}

func (v *valueVisitor) VisitU64(value uint64) (any, error) {
	*v.out = NumberFromU64(value).Value()
	return nil, nil
}

func (v *valueVisitor) VisitF64(value float64) (any, error) {
	if math.IsNaN(value) || math.IsInf(value, 0) {
		*v.out = nil
	} else {
		*v.out = value
	}
	return nil, nil
}

func (v *valueVisitor) VisitStr(value string) (any, error) {
	*v.out = value
	return nil, nil
}

func (v *valueVisitor) VisitNone() (any, error) {
	*v.out = nil
	return nil, nil
}

func (v *valueVisitor) VisitSome(deserializer serde.Deserializer) (any, error) {
	value, err := deserializeValue(deserializer)
	if err != nil {
		return nil, err
	}
	*v.out = value
	return nil, nil
}

func (v *valueVisitor) VisitUnit() (any, error) {
	*v.out = nil
	return nil, nil
}

func (v *valueVisitor) VisitSeq(seq serde.DeSeqAccess) (any, error) {
	vec := []any{}
	for {
		var element Value
		_, ok, err := seq.NextElementSeed(&valueSeed{out: &element})
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		vec = append(vec, element)
	}
	*v.out = vec
	return nil, nil
}

func (v *valueVisitor) VisitMap(m serde.DeMapAccess) (any, error) {
	key, ok, err := m.NextKeySeed(keyClassifier{})
	if err != nil {
		return nil, err
	}
	if !ok {
		*v.out = Map{}
		return nil, nil
	}
	if key.(keyClass).rawValue {
		var raw RawValue
		if _, err := m.NextValueSeed(&boxedFromString{out: &raw}); err != nil {
			return nil, err
		}
		value, err := FromStr[Value](raw.Get())
		if err != nil {
			return nil, serde.DeErrorCustom(err)
		}
		*v.out = value
		return nil, nil
	}
	values := mapBuilder{}
	var first Value
	if _, err := m.NextValueSeed(&valueSeed{out: &first}); err != nil {
		return nil, err
	}
	values.insert(key.(keyClass).key, first)
	for {
		var key string
		var value Value
		_, _, ok, err := m.NextEntrySeed(serde.SeedFor(&key), &valueSeed{out: &value})
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		values.insert(key, value)
	}
	*v.out = values.m
	return nil, nil
}

// valueSeed deserializes a Value into out.
type valueSeed struct{ out *Value }

func (s *valueSeed) DeserializeSeed(deserializer serde.Deserializer) (any, error) {
	value, err := deserializeValue(deserializer)
	if err != nil {
		return nil, err
	}
	*s.out = value
	return nil, nil
}

// mapBuilder inserts into a Map as `Map::insert` does, finding keys through
// an index once the map is large, so that building stays linear.
type mapBuilder struct {
	m     Map
	index map[string]int
}

func (b *mapBuilder) insert(key string, value Value) {
	if b.m == nil {
		b.m = Map{}
	}
	if b.index != nil {
		if i, ok := b.index[key]; ok {
			b.m[i].Value = value
			return
		}
	} else {
		for i := range b.m {
			if b.m[i].Key == key {
				b.m[i].Value = value
				return
			}
		}
	}
	b.m = append(b.m, jsonvalue.Member{Key: key, Value: value})
	switch {
	case b.index != nil:
		b.index[key] = len(b.m) - 1
	case len(b.m) > 16:
		b.index = make(map[string]int, 2*len(b.m))
		for i, member := range b.m {
			b.index[member.Key] = i
		}
	}
}

// keyClass is `KeyClass`.
type keyClass struct {
	key      string
	rawValue bool
}

// keyClassifier is `KeyClassifier`.
type keyClassifier struct{}

func (keyClassifier) DeserializeSeed(deserializer serde.Deserializer) (any, error) {
	return deserializer.DeserializeStr(keyClassifier{})
}

func (keyClassifier) Expecting() string { return "a string key" }

func (keyClassifier) VisitStr(s string) (any, error) {
	if s == rawValueToken {
		return keyClass{rawValue: true}, nil
	}
	return keyClass{key: s}, nil
}

// mapVisitor is the visitor of `impl Deserialize for Map<String, Value>`.
type mapVisitor struct{ out *Map }

func (v *mapVisitor) Expecting() string { return "a map" }

func (v *mapVisitor) VisitUnit() (any, error) {
	*v.out = Map{}
	return nil, nil
}

func (v *mapVisitor) VisitMap(m serde.DeMapAccess) (any, error) {
	values := mapBuilder{m: Map{}}
	for {
		var key string
		var value Value
		_, _, ok, err := m.NextEntrySeed(serde.SeedFor(&key), &valueSeed{out: &value})
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		values.insert(key, value)
	}
	*v.out = values.m
	return nil, nil
}

// ToValue is `serde_json::to_value`: convert a value into a Value. A nil is
// Value::Null.
func ToValue(value any) (Value, error) {
	if value == nil {
		return nil, nil
	}
	result, err := serde.Serialize(value, valueSerializer{})
	if err != nil {
		return nil, toError(err)
	}
	return result, nil
}

// FromValue is `serde_json::from_value`: interpret a Value as an instance
// of type T.
func FromValue[T any](value Value) (T, error) {
	result, err := serde.Deserialize[T](valueDeserializer{v: value})
	if err != nil {
		var zero T
		return zero, toError(err)
	}
	return result, nil
}

// toValueElement is `to_value` inside the value serializer.
func toValueElement(value any) (Value, error) {
	if r, ok := value.(valueRef); ok {
		return valueOfValue(r.v)
	}
	return ToValue(value)
}

// valueOfValue converts a Value that may hold other Go values into the
// jsonvalue representation.
func valueOfValue(v Value) (Value, error) {
	result, err := serializeValue(v, valueSerializer{})
	if err != nil {
		return nil, toError(err)
	}
	return result, nil
}

// valueSerializer is `serde_json::value::Serializer`: it builds the Value
// of what it is given.
type valueSerializer struct{}

func (valueSerializer) SerializeBool(v bool) (any, error)   { return v, nil }
func (valueSerializer) SerializeI8(v int8) (any, error)     { return int64(v), nil }
func (valueSerializer) SerializeI16(v int16) (any, error)   { return int64(v), nil }
func (valueSerializer) SerializeI32(v int32) (any, error)   { return int64(v), nil }
func (valueSerializer) SerializeI64(v int64) (any, error)   { return v, nil }
func (valueSerializer) SerializeU8(v uint8) (any, error)    { return int64(v), nil }
func (valueSerializer) SerializeU16(v uint16) (any, error)  { return int64(v), nil }
func (valueSerializer) SerializeU32(v uint32) (any, error)  { return int64(v), nil }
func (valueSerializer) SerializeU64(v uint64) (any, error)  { return NumberFromU64(v).Value(), nil }
func (valueSerializer) SerializeF32(v float32) (any, error) { return ValueFromF64(float64(v)), nil }
func (valueSerializer) SerializeF64(v float64) (any, error) { return ValueFromF64(v), nil }
func (valueSerializer) SerializeChar(v rune) (any, error)   { return string(v), nil }
func (valueSerializer) SerializeStr(v string) (any, error)  { return v, nil }

func (valueSerializer) SerializeBytes(v []byte) (any, error) {
	vec := make([]any, len(v))
	for i, b := range v {
		vec[i] = int64(b)
	}
	return vec, nil
}

func (valueSerializer) SerializeUnit() (any, error)             { return nil, nil }
func (valueSerializer) SerializeUnitStruct(string) (any, error) { return nil, nil }
func (valueSerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return variant, nil
}

func (s valueSerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return serde.Serialize(value, s)
}

func (valueSerializer) SerializeNewtypeVariant(_ string, _ uint32, variant string, value any) (any, error) {
	inner, err := toValueElement(value)
	if err != nil {
		return nil, err
	}
	return Map{{Key: variant, Value: inner}}, nil
}

func (valueSerializer) SerializeNone() (any, error) { return nil, nil }

func (s valueSerializer) SerializeSome(value any) (any, error) { return serde.Serialize(value, s) }

func (valueSerializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	return &serializeVec{vec: make([]any, 0, length.UnwrapOr(0))}, nil
}

func (valueSerializer) SerializeTuple(length int) (serde.SerSerializeTuple, error) {
	return &serializeVec{vec: make([]any, 0, length)}, nil
}

func (valueSerializer) SerializeTupleStruct(_ string, length int) (serde.SerSerializeTupleStruct, error) {
	return &serializeVec{vec: make([]any, 0, length)}, nil
}

func (valueSerializer) SerializeTupleVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeTupleVariant, error) {
	return &serializeTupleVariant{name: variant, vec: make([]any, 0, length)}, nil
}

func (valueSerializer) SerializeMap(length rs.Option[int]) (serde.SerSerializeMap, error) {
	return &serializeValueMap{builder: mapBuilder{m: make(Map, 0, length.UnwrapOr(0))}}, nil
}

func (s valueSerializer) SerializeStruct(name string, length int) (serde.SerSerializeStruct, error) {
	if name == rawValueToken {
		return &serializeValueMap{raw: true}, nil
	}
	return &serializeValueMap{builder: mapBuilder{m: make(Map, 0, length)}}, nil
}

func (valueSerializer) SerializeStructVariant(_ string, _ uint32, variant string, _ int) (serde.SerSerializeStructVariant, error) {
	return &serializeStructVariant{name: variant, builder: mapBuilder{m: Map{}}}, nil
}

func (valueSerializer) CollectStr(value string) (any, error) { return value, nil }
func (valueSerializer) IsHumanReadable() bool                { return true }

// serializeVec is `SerializeVec`.
type serializeVec struct{ vec []any }

func (s *serializeVec) SerializeElement(value any) error {
	element, err := toValueElement(value)
	if err != nil {
		return err
	}
	s.vec = append(s.vec, element)
	return nil
}

func (s *serializeVec) SerializeField(value any) error { return s.SerializeElement(value) }
func (s *serializeVec) End() (any, error)              { return s.vec, nil }

// serializeTupleVariant is `SerializeTupleVariant`.
type serializeTupleVariant struct {
	name string
	vec  []any
}

func (s *serializeTupleVariant) SerializeField(value any) error {
	element, err := toValueElement(value)
	if err != nil {
		return err
	}
	s.vec = append(s.vec, element)
	return nil
}

func (s *serializeTupleVariant) End() (any, error) {
	return Map{{Key: s.name, Value: s.vec}}, nil
}

// serializeValueMap is `SerializeMap`, and its `RawValue` form.
type serializeValueMap struct {
	builder    mapBuilder
	nextKey    string
	hasNextKey bool
	raw        bool
	outValue   Value
	hasOut     bool
}

func (s *serializeValueMap) SerializeKey(key any) error {
	k, err := serde.Serialize(key, valueMapKeySerializer{})
	if err != nil {
		return toError(err)
	}
	s.nextKey, s.hasNextKey = k.(string), true
	return nil
}

func (s *serializeValueMap) SerializeValue(value any) error {
	if !s.hasNextKey {
		// Panic because this indicates a bug in the program rather than an
		// expected failure.
		panic(rs.Panicf("serialize_value called before serialize_key"))
	}
	key := s.nextKey
	s.hasNextKey = false
	element, err := toValueElement(value)
	if err != nil {
		return err
	}
	s.builder.insert(key, element)
	return nil
}

func (s *serializeValueMap) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(s, key, value)
}

func (s *serializeValueMap) SerializeField(key string, value any) error {
	if s.raw {
		if key != rawValueToken {
			return errorSyntax(errorCodeExpectedSomeValue, 0, 0)
		}
		out, err := serde.Serialize(value, rawValueEmitter{})
		if err != nil {
			return toError(err)
		}
		s.outValue, s.hasOut = out, true
		return nil
	}
	return s.SerializeEntry(key, value)
}

func (s *serializeValueMap) SkipField(string) error { return nil }

func (s *serializeValueMap) End() (any, error) {
	if s.raw {
		if !s.hasOut {
			panic(rs.Panicf("raw value was not emitted"))
		}
		return s.outValue, nil
	}
	if s.builder.m == nil {
		return Map{}, nil
	}
	return s.builder.m, nil
}

// serializeStructVariant is `SerializeStructVariant`.
type serializeStructVariant struct {
	name    string
	builder mapBuilder
}

func (s *serializeStructVariant) SerializeField(key string, value any) error {
	element, err := toValueElement(value)
	if err != nil {
		return err
	}
	s.builder.insert(key, element)
	return nil
}

func (s *serializeStructVariant) SkipField(string) error { return nil }

func (s *serializeStructVariant) End() (any, error) {
	return Map{{Key: s.name, Value: s.builder.m}}, nil
}

// valueMapKeySerializer is the `MapKeySerializer` of the value serializer:
// a key becomes a string.
type valueMapKeySerializer struct{}

func (valueMapKeySerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return variant, nil
}

func (k valueMapKeySerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return serde.Serialize(value, k)
}

func (valueMapKeySerializer) SerializeBool(v bool) (any, error) {
	if v {
		return "true", nil
	}
	return "false", nil
}

func (valueMapKeySerializer) SerializeI8(v int8) (any, error) {
	return strconv.FormatInt(int64(v), 10), nil
}
func (valueMapKeySerializer) SerializeI16(v int16) (any, error) {
	return strconv.FormatInt(int64(v), 10), nil
}
func (valueMapKeySerializer) SerializeI32(v int32) (any, error) {
	return strconv.FormatInt(int64(v), 10), nil
}
func (valueMapKeySerializer) SerializeI64(v int64) (any, error) {
	return strconv.FormatInt(v, 10), nil
}
func (valueMapKeySerializer) SerializeU8(v uint8) (any, error) {
	return strconv.FormatUint(uint64(v), 10), nil
}
func (valueMapKeySerializer) SerializeU16(v uint16) (any, error) {
	return strconv.FormatUint(uint64(v), 10), nil
}
func (valueMapKeySerializer) SerializeU32(v uint32) (any, error) {
	return strconv.FormatUint(uint64(v), 10), nil
}
func (valueMapKeySerializer) SerializeU64(v uint64) (any, error) {
	return strconv.FormatUint(v, 10), nil
}

func (valueMapKeySerializer) SerializeF32(v float32) (any, error) {
	if math.IsNaN(float64(v)) || math.IsInf(float64(v), 0) {
		return nil, floatKeyMustBeFinite()
	}
	return formatF32(v), nil
}

func (valueMapKeySerializer) SerializeF64(v float64) (any, error) {
	if math.IsNaN(v) || math.IsInf(v, 0) {
		return nil, floatKeyMustBeFinite()
	}
	return formatF64(v), nil
}

func (valueMapKeySerializer) SerializeChar(v rune) (any, error)  { return string(v), nil }
func (valueMapKeySerializer) SerializeStr(v string) (any, error) { return v, nil }
func (valueMapKeySerializer) SerializeBytes([]byte) (any, error) { return nil, keyMustBeAString() }
func (valueMapKeySerializer) SerializeUnit() (any, error)        { return nil, keyMustBeAString() }
func (valueMapKeySerializer) SerializeUnitStruct(string) (any, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeNewtypeVariant(string, uint32, string, any) (any, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeNone() (any, error)    { return nil, keyMustBeAString() }
func (valueMapKeySerializer) SerializeSome(any) (any, error) { return nil, keyMustBeAString() }
func (valueMapKeySerializer) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeTupleStruct(string, int) (serde.SerSerializeTupleStruct, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeTupleVariant(string, uint32, string, int) (serde.SerSerializeTupleVariant, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeStruct(string, int) (serde.SerSerializeStruct, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) SerializeStructVariant(string, uint32, string, int) (serde.SerSerializeStructVariant, error) {
	return nil, keyMustBeAString()
}
func (valueMapKeySerializer) CollectStr(value string) (any, error) { return value, nil }
func (valueMapKeySerializer) IsHumanReadable() bool                { return true }

// rawValueEmitter is `RawValueEmitter`: the text of a RawValue, parsed into
// the Value it holds.
type rawValueEmitter struct{}

func (rawValueEmitter) SerializeStr(value string) (any, error) { return FromStr[Value](value) }

func (rawValueEmitter) SerializeBool(bool) (any, error)         { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeI8(int8) (any, error)           { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeI16(int16) (any, error)         { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeI32(int32) (any, error)         { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeI64(int64) (any, error)         { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeU8(uint8) (any, error)          { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeU16(uint16) (any, error)        { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeU32(uint32) (any, error)        { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeU64(uint64) (any, error)        { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeF32(float32) (any, error)       { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeF64(float64) (any, error)       { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeChar(rune) (any, error)         { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeBytes([]byte) (any, error)      { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeNone() (any, error)             { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeSome(any) (any, error)          { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeUnit() (any, error)             { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeUnitStruct(string) (any, error) { return nil, expectedRawValue() }
func (rawValueEmitter) SerializeUnitVariant(string, uint32, string) (any, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeNewtypeStruct(string, any) (any, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeNewtypeVariant(string, uint32, string, any) (any, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeTupleStruct(string, int) (serde.SerSerializeTupleStruct, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeTupleVariant(string, uint32, string, int) (serde.SerSerializeTupleVariant, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeStruct(string, int) (serde.SerSerializeStruct, error) {
	return nil, expectedRawValue()
}
func (rawValueEmitter) SerializeStructVariant(string, uint32, string, int) (serde.SerSerializeStructVariant, error) {
	return nil, expectedRawValue()
}
func (e rawValueEmitter) CollectStr(value string) (any, error) { return e.SerializeStr(value) }
func (rawValueEmitter) IsHumanReadable() bool                  { return true }

// valueDeserializer is `impl Deserializer for Value`: a Value read as the
// input of a Deserialize impl. owned is false for `&Value`, which hands
// strings over as borrowed.
type valueDeserializer struct {
	v        Value
	borrowed bool
}

// ValueDeserializer returns the serde.Deserializer of a Value, which is
// `value.into_deserializer()` and `Value` as a Deserializer.
func ValueDeserializer(value Value) serde.Deserializer { return valueDeserializer{v: value} }

// ValueRefDeserializer returns the serde.Deserializer of `&Value`.
func ValueRefDeserializer(value Value) serde.Deserializer {
	return valueDeserializer{v: value, borrowed: true}
}

// unexpected is `Value::unexpected`.
func valueUnexpected(v Value) serde.DeUnexpected {
	switch x := v.(type) {
	case nil:
		return &serde.DeUnexpectedUnit{}
	case bool:
		return &serde.DeUnexpectedBool{F0: x}
	case string:
		return &serde.DeUnexpectedStr{F0: x}
	case []any:
		return &serde.DeUnexpectedSeq{}
	case Map:
		return &serde.DeUnexpectedMap{}
	}
	if n, ok := numberOf(v); ok {
		return n.unexpected()
	}
	return &serde.DeUnexpectedOther{F0: reflect.TypeOf(v).String()}
}

func (d valueDeserializer) invalidType(exp serde.DeExpected) error {
	return toError(serde.DeErrorInvalidType(valueUnexpected(d.v), exp))
}

func (d valueDeserializer) visitString(visitor serde.DeVisitor, s string) (any, error) {
	if d.borrowed {
		return leaveAsError(serde.DeVisitorVisitBorrowedStr(visitor, s))
	}
	return leaveAsError(serde.DeVisitorVisitString(visitor, s))
}

func (d valueDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	switch x := d.v.(type) {
	case nil:
		return leaveAsError(serde.DeVisitorVisitUnit(visitor))
	case bool:
		return leaveAsError(serde.DeVisitorVisitBool(visitor, x))
	case string:
		return d.visitString(visitor, x)
	case []any:
		return visitArray(x, d.borrowed, visitor)
	case Map:
		return visitObject(x, d.borrowed, visitor)
	}
	if n, ok := numberOf(d.v); ok {
		return leaveAsError(n.visit(visitor))
	}
	normal, err := valueOfValue(d.v)
	if err != nil {
		return nil, err
	}
	return valueDeserializer{v: normal, borrowed: d.borrowed}.DeserializeAny(visitor)
}

func (d valueDeserializer) number(visitor serde.DeVisitor) (any, error) {
	if n, ok := numberOf(d.v); ok {
		return leaveAsError(n.visit(visitor))
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeBool(visitor serde.DeVisitor) (any, error) {
	if b, ok := d.v.(bool); ok {
		return leaveAsError(serde.DeVisitorVisitBool(visitor, b))
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeI8(v serde.DeVisitor) (any, error)  { return d.number(v) }
func (d valueDeserializer) DeserializeI16(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeI32(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeI64(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeU8(v serde.DeVisitor) (any, error)  { return d.number(v) }
func (d valueDeserializer) DeserializeU16(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeU32(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeU64(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeF32(v serde.DeVisitor) (any, error) { return d.number(v) }
func (d valueDeserializer) DeserializeF64(v serde.DeVisitor) (any, error) { return d.number(v) }

func (d valueDeserializer) DeserializeChar(v serde.DeVisitor) (any, error) {
	return d.DeserializeString(v)
}

func (d valueDeserializer) DeserializeStr(v serde.DeVisitor) (any, error) {
	return d.DeserializeString(v)
}

func (d valueDeserializer) DeserializeString(visitor serde.DeVisitor) (any, error) {
	if s, ok := d.v.(string); ok {
		return d.visitString(visitor, s)
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeBytes(v serde.DeVisitor) (any, error) {
	return d.DeserializeByteBuf(v)
}

func (d valueDeserializer) DeserializeByteBuf(visitor serde.DeVisitor) (any, error) {
	switch x := d.v.(type) {
	case string:
		return d.visitString(visitor, x)
	case []any:
		return visitArray(x, d.borrowed, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	if d.v == nil {
		return leaveAsError(serde.DeVisitorVisitNone(visitor))
	}
	return leaveAsError(serde.DeVisitorVisitSome(visitor, d))
}

func (d valueDeserializer) DeserializeUnit(visitor serde.DeVisitor) (any, error) {
	if d.v == nil {
		return leaveAsError(serde.DeVisitorVisitUnit(visitor))
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeUnitStruct(_ string, visitor serde.DeVisitor) (any, error) {
	return d.DeserializeUnit(visitor)
}

func (d valueDeserializer) DeserializeNewtypeStruct(name string, visitor serde.DeVisitor) (any, error) {
	if name == rawValueToken {
		text, err := ToString(valueRef{d.v})
		if err != nil {
			return nil, err
		}
		return leaveAsError(serde.DeVisitorVisitMap(visitor, &rawDeserializer{rawValue: text, present: true}))
	}
	return leaveAsError(serde.DeVisitorVisitNewtypeStruct(visitor, d))
}

func (d valueDeserializer) DeserializeSeq(visitor serde.DeVisitor) (any, error) {
	if x, ok := d.v.([]any); ok {
		return visitArray(x, d.borrowed, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeTuple(_ int, v serde.DeVisitor) (any, error) {
	return d.DeserializeSeq(v)
}

func (d valueDeserializer) DeserializeTupleStruct(_ string, _ int, v serde.DeVisitor) (any, error) {
	return d.DeserializeSeq(v)
}

func (d valueDeserializer) DeserializeMap(visitor serde.DeVisitor) (any, error) {
	if x, ok := d.v.(Map); ok {
		return visitObject(x, d.borrowed, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeStruct(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	switch x := d.v.(type) {
	case []any:
		return visitArray(x, d.borrowed, visitor)
	case Map:
		return visitObject(x, d.borrowed, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d valueDeserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	switch x := d.v.(type) {
	case Map:
		x = mapMerged(x)
		// enums are encoded in json as maps with a single key:value pair
		if len(x) != 1 {
			return nil, toError(serde.DeErrorInvalidValue(&serde.DeUnexpectedMap{}, serde.DeExpectedStr("map with a single key")))
		}
		return leaveAsError(serde.DeVisitorVisitEnum(visitor, &valueEnumDeserializer{variant: x[0].Key, value: x[0].Value, hasValue: true, borrowed: d.borrowed}))
	case string:
		return leaveAsError(serde.DeVisitorVisitEnum(visitor, &valueEnumDeserializer{variant: x, borrowed: d.borrowed}))
	}
	return nil, toError(serde.DeErrorInvalidType(valueUnexpected(d.v), serde.DeExpectedStr("string or map")))
}

func (d valueDeserializer) DeserializeIdentifier(v serde.DeVisitor) (any, error) {
	return d.DeserializeString(v)
}

func (d valueDeserializer) DeserializeIgnoredAny(visitor serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitUnit(visitor))
}

func (d valueDeserializer) IsHumanReadable() bool { return true }

// visitArray is `visit_array` and `visit_array_ref`.
func visitArray(array []any, borrowed bool, visitor serde.DeVisitor) (any, error) {
	seq := &valueSeqDeserializer{items: array, borrowed: borrowed}
	value, err := serde.DeVisitorVisitSeq(visitor, seq)
	if err != nil {
		return nil, toError(err)
	}
	if seq.next == len(array) {
		return value, nil
	}
	return nil, toError(serde.DeErrorInvalidLength(len(array), serde.DeExpectedStr("fewer elements in array")))
}

// visitObject is `Map::deserialize_any`.
func visitObject(m Map, borrowed bool, visitor serde.DeVisitor) (any, error) {
	m = mapMerged(m)
	access := &valueMapDeserializer{entries: m, borrowed: borrowed}
	value, err := serde.DeVisitorVisitMap(visitor, access)
	if err != nil {
		return nil, toError(err)
	}
	if access.next == len(m) {
		return value, nil
	}
	return nil, toError(serde.DeErrorInvalidLength(len(m), serde.DeExpectedStr("fewer elements in map")))
}

// valueSeqDeserializer is `SeqDeserializer` and `SeqRefDeserializer`.
type valueSeqDeserializer struct {
	items    []any
	next     int
	borrowed bool
}

func (s *valueSeqDeserializer) NextElementSeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if s.next >= len(s.items) {
		return nil, false, nil
	}
	item := s.items[s.next]
	s.next++
	value, err := seed.DeserializeSeed(valueDeserializer{v: item, borrowed: s.borrowed})
	if err != nil {
		return nil, false, toError(err)
	}
	return value, true, nil
}

func (s *valueSeqDeserializer) SizeHint() rs.Option[int] { return rs.Some(len(s.items) - s.next) }

// valueMapDeserializer is `MapDeserializer` and `MapRefDeserializer`.
type valueMapDeserializer struct {
	entries  Map
	next     int
	value    Value
	hasValue bool
	borrowed bool
}

func (m *valueMapDeserializer) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if m.next >= len(m.entries) {
		return nil, false, nil
	}
	entry := m.entries[m.next]
	m.next++
	m.value, m.hasValue = entry.Value, true
	key, err := seed.DeserializeSeed(valueMapKeyDeserializer{key: entry.Key, borrowed: m.borrowed})
	if err != nil {
		return nil, false, toError(err)
	}
	return key, true, nil
}

func (m *valueMapDeserializer) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	if !m.hasValue {
		return nil, toError(serde.DeErrorCustom("value is missing"))
	}
	value := m.value
	m.value, m.hasValue = nil, false
	return leaveAsError(seed.DeserializeSeed(valueDeserializer{v: value, borrowed: m.borrowed}))
}

func (m *valueMapDeserializer) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(m, keySeed, valueSeed)
}

func (m *valueMapDeserializer) SizeHint() rs.Option[int] { return rs.Some(len(m.entries) - m.next) }

// valueEnumDeserializer is `EnumDeserializer` and its `VariantDeserializer`.
type valueEnumDeserializer struct {
	variant  string
	value    Value
	hasValue bool
	borrowed bool
}

func (e *valueEnumDeserializer) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	var d serde.Deserializer = stringDeserializer(e.variant)
	if e.borrowed {
		d = borrowedStrDeserializer(e.variant)
	}
	value, err := seed.DeserializeSeed(d)
	if err != nil {
		return nil, nil, toError(err)
	}
	return value, e, nil
}

func (e *valueEnumDeserializer) UnitVariant() error {
	if !e.hasValue {
		return nil
	}
	_, err := serde.Deserialize[rs.Unit](valueDeserializer{v: e.value, borrowed: e.borrowed})
	if err != nil {
		return toError(err)
	}
	return nil
}

func (e *valueEnumDeserializer) NewtypeVariantSeed(seed serde.DeDeserializeSeed) (any, error) {
	if !e.hasValue {
		return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("newtype variant")))
	}
	return leaveAsError(seed.DeserializeSeed(valueDeserializer{v: e.value, borrowed: e.borrowed}))
}

func (e *valueEnumDeserializer) TupleVariant(_ int, visitor serde.DeVisitor) (any, error) {
	if !e.hasValue {
		return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("tuple variant")))
	}
	if array, ok := e.value.([]any); ok {
		if len(array) == 0 {
			return leaveAsError(serde.DeVisitorVisitUnit(visitor))
		}
		return visitArray(array, e.borrowed, visitor)
	}
	return nil, toError(serde.DeErrorInvalidType(valueUnexpected(e.value), serde.DeExpectedStr("tuple variant")))
}

func (e *valueEnumDeserializer) StructVariant(_ []string, visitor serde.DeVisitor) (any, error) {
	if !e.hasValue {
		return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("struct variant")))
	}
	if m, ok := e.value.(Map); ok {
		return visitObject(m, e.borrowed, visitor)
	}
	return nil, toError(serde.DeErrorInvalidType(valueUnexpected(e.value), serde.DeExpectedStr("struct variant")))
}

// valueMapKeyDeserializer is `MapKeyDeserializer`: an object key, which can
// also be read as a number or a bool written in it.
type valueMapKeyDeserializer struct {
	key      string
	borrowed bool
}

func (k valueMapKeyDeserializer) str() serde.Deserializer {
	if k.borrowed {
		return borrowedStrDeserializer(k.key)
	}
	return stringDeserializer(k.key)
}

// numericKey is `deserialize_numeric_key!`.
func (k valueMapKeyDeserializer) numericKey(visitor serde.DeVisitor, f32 bool) (any, error) {
	de := DeserializerFromStr(k.key)
	c, ok, err := de.peek()
	if err != nil {
		return nil, err
	}
	if !ok || !(c >= '0' && c <= '9' || c == '-') {
		return nil, errorSyntax(errorCodeExpectedNumericKey, 0, 0)
	}
	var number any
	if f32 {
		number, err = de.doDeserializeF32(visitor)
	} else {
		number, err = de.deserializeNumber(visitor)
	}
	if err != nil {
		return nil, err
	}
	if _, more, err := de.peek(); err != nil {
		return nil, err
	} else if more {
		return nil, errorSyntax(errorCodeExpectedNumericKey, 0, 0)
	}
	return number, nil
}

func (k valueMapKeyDeserializer) DeserializeAny(v serde.DeVisitor) (any, error) {
	return k.str().DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeI8(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeI16(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeI32(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeI64(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeU8(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeU16(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeU32(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeU64(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}
func (k valueMapKeyDeserializer) DeserializeF32(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, true)
}
func (k valueMapKeyDeserializer) DeserializeF64(v serde.DeVisitor) (any, error) {
	return k.numericKey(v, false)
}

func (k valueMapKeyDeserializer) DeserializeBool(visitor serde.DeVisitor) (any, error) {
	switch k.key {
	case "true":
		return leaveAsError(serde.DeVisitorVisitBool(visitor, true))
	case "false":
		return leaveAsError(serde.DeVisitorVisitBool(visitor, false))
	}
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedStr{F0: k.key}, visitor))
}

func (k valueMapKeyDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	// Map keys cannot be null.
	return leaveAsError(serde.DeVisitorVisitSome(visitor, k))
}

func (k valueMapKeyDeserializer) DeserializeNewtypeStruct(_ string, visitor serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitNewtypeStruct(visitor, k))
}

func (k valueMapKeyDeserializer) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	if k.borrowed {
		return leaveAsError(serde.DeVisitorVisitEnum(visitor, &unitOnlyEnum{variant: k.key}))
	}
	return stringDeserializer(k.key).DeserializeEnum(name, variants, visitor)
}

func (k valueMapKeyDeserializer) DeserializeChar(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeStr(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeString(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeBytes(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeByteBuf(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeUnit(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeUnitStruct(_ string, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeSeq(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeTuple(_ int, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeTupleStruct(_ string, _ int, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeMap(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeStruct(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeIdentifier(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) DeserializeIgnoredAny(v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k valueMapKeyDeserializer) IsHumanReadable() bool { return true }

// stringDeserializer is serde's `StringDeserializer` with this crate's
// error (`String::into_deserializer`): an owned string.
type stringDeserializer string

func (d stringDeserializer) any(v serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitString(v, string(d)))
}

func (d stringDeserializer) DeserializeAny(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeBool(v serde.DeVisitor) (any, error)       { return d.any(v) }
func (d stringDeserializer) DeserializeI8(v serde.DeVisitor) (any, error)         { return d.any(v) }
func (d stringDeserializer) DeserializeI16(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeI32(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeI64(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeU8(v serde.DeVisitor) (any, error)         { return d.any(v) }
func (d stringDeserializer) DeserializeU16(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeU32(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeU64(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeF32(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeF64(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeChar(v serde.DeVisitor) (any, error)       { return d.any(v) }
func (d stringDeserializer) DeserializeStr(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeString(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d stringDeserializer) DeserializeBytes(v serde.DeVisitor) (any, error)      { return d.any(v) }
func (d stringDeserializer) DeserializeByteBuf(v serde.DeVisitor) (any, error)    { return d.any(v) }
func (d stringDeserializer) DeserializeOption(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d stringDeserializer) DeserializeUnit(v serde.DeVisitor) (any, error)       { return d.any(v) }
func (d stringDeserializer) DeserializeSeq(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeMap(v serde.DeVisitor) (any, error)        { return d.any(v) }
func (d stringDeserializer) DeserializeIdentifier(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d stringDeserializer) DeserializeIgnoredAny(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d stringDeserializer) DeserializeUnitStruct(_ string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d stringDeserializer) DeserializeNewtypeStruct(_ string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d stringDeserializer) DeserializeTuple(_ int, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d stringDeserializer) DeserializeTupleStruct(_ string, _ int, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d stringDeserializer) DeserializeStruct(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}

// DeserializeEnum is `StringDeserializer::deserialize_enum`: the string is
// a unit variant.
func (d stringDeserializer) DeserializeEnum(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitEnum(v, &unitOnlyEnum{variant: string(d), owned: true}))
}
func (d stringDeserializer) IsHumanReadable() bool { return true }

var (
	_ serde.Deserializer = valueDeserializer{}
	_ serde.Deserializer = valueMapKeyDeserializer{}
	_ serde.Deserializer = stringDeserializer("")
	_ serde.Serializer   = valueSerializer{}
	_ serde.Serializer   = valueMapKeySerializer{}
	_ serde.Serializer   = rawValueEmitter{}
)
