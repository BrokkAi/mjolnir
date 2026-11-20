// Port of toml_edit 0.20.2 src/ser/map.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// serSerializeMap is SerializeMap, an enum of two variants: exactly one of
// datetime (SerializeMap::Datetime) and table (SerializeMap::Table) is set.
// It is the SerializeMap and SerializeStruct of ValueSerializer; its `Ok` is
// a Value.
type serSerializeMap struct {
	datetime *serSerializeDatetime
	table    *serSerializeInlineTable
}

func serSerializeMapTable() *serSerializeMap {
	return &serSerializeMap{datetime: nil, table: serSerializeInlineTableNew()}
}

func serSerializeMapTableWithCapacity(length int) *serSerializeMap {
	return &serSerializeMap{datetime: nil, table: serSerializeInlineTableWithCapacity(length)}
}

func serSerializeMapDatetime() *serSerializeMap {
	return &serSerializeMap{datetime: serSerializeDatetimeNew(), table: nil}
}

// SerializeKey is `SerializeMap::serialize_key`.
func (s *serSerializeMap) SerializeKey(input any) error {
	if s.datetime != nil {
		return s.datetime.serializeMapKey(input)
	}
	return s.table.SerializeKey(input)
}

// SerializeValue is `SerializeMap::serialize_value`.
func (s *serSerializeMap) SerializeValue(value any) error {
	if s.datetime != nil {
		return s.datetime.serializeMapValue(value)
	}
	return s.table.SerializeValue(value)
}

// SerializeEntry is the default `SerializeMap::serialize_entry`.
func (s *serSerializeMap) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(s, key, value)
}

// End is `SerializeStruct::end`, and `SerializeMap::end`. Rust has one of
// each, and they differ for a datetime, whose SerializeMap::end is
// `unreachable!`. Go has one method for both; it is the SerializeStruct one,
// because only serialize_struct makes a datetime, so the SerializeMap end is
// never called on one.
func (s *serSerializeMap) End() (any, error) {
	if s.datetime != nil {
		items, err := s.datetime.endStruct()
		if err != nil {
			return nil, err
		}
		return ValueFromDatetime(items), nil
	}
	items, err := s.table.End()
	if err != nil {
		return nil, err
	}
	return ValueFromInlineTable(items), nil
}

// SerializeField is `SerializeStruct::serialize_field`.
func (s *serSerializeMap) SerializeField(key string, value any) error {
	if s.datetime != nil {
		return s.datetime.SerializeField(key, value)
	}
	return s.table.SerializeField(key, value)
}

// SkipField is the default `SerializeStruct::skip_field`.
func (s *serSerializeMap) SkipField(string) error { return nil }

// serSerializeDatetime is SerializeDatetime.
type serSerializeDatetime struct {
	value rs.Option[Datetime]
}

func serSerializeDatetimeNew() *serSerializeDatetime {
	return &serSerializeDatetime{value: rs.None[Datetime]()}
}

// serializeMapKey is `impl SerializeMap for SerializeDatetime`.
func (s *serSerializeDatetime) serializeMapKey(any) error {
	panic(rs.Unreachable("datetimes should only be serialized as structs, not maps"))
}

func (s *serSerializeDatetime) serializeMapValue(any) error {
	panic(rs.Unreachable("datetimes should only be serialized as structs, not maps"))
}

// SerializeField is `impl SerializeStruct for SerializeDatetime`.
func (s *serSerializeDatetime) SerializeField(key string, value any) error {
	if key == DatetimeField {
		datetime, err := serde.Serialize(value, serDatetimeFieldSerializer{})
		if err != nil {
			return serErrorFrom(err)
		}
		s.value = rs.Some(datetime.(Datetime))
	}

	return nil
}

func (s *serSerializeDatetime) endStruct() (Datetime, error) {
	return s.value.OkOr(&SerError_UnsupportedNone{})
}

// serSerializeInlineTable is SerializeInlineTable. Its `Ok` is an
// InlineTable.
type serSerializeInlineTable struct {
	items keyValuePairs
	key   rs.Option[InternalString]
}

func serSerializeInlineTableNew() *serSerializeInlineTable {
	return &serSerializeInlineTable{items: keyValuePairs{}, key: rs.None[InternalString]()}
}

func serSerializeInlineTableWithCapacity(length int) *serSerializeInlineTable {
	s := serSerializeInlineTableNew()
	s.items.entries = make([]*tableKeyValue, 0, length)
	return s
}

// SerializeKey is `impl SerializeMap for SerializeInlineTable`.
func (s *serSerializeInlineTable) SerializeKey(input any) error {
	key, err := serde.Serialize(input, serKeySerializer{})
	if err != nil {
		return serErrorFrom(err)
	}
	s.key = rs.Some(key.(InternalString))
	return nil
}

func (s *serSerializeInlineTable) SerializeValue(value any) error {
	valueSerializer := mapValueSerializerNew()
	res, err := serde.Serialize(value, valueSerializer)
	if err == nil {
		key := s.key.Take().Unwrap()
		kv := tableKeyValueNew(KeyNew(key), &Item_Value{F0: res.(Value)})
		s.items.insert(key, kv)
	} else {
		e := serErrorFrom(err)
		if _, isNone := e.(*SerError_UnsupportedNone); !(isNone && valueSerializer.isNone) {
			return e
		}
	}
	return nil
}

func (s *serSerializeInlineTable) End() (InlineTable, error) {
	return inlineTableWithPairs(s.items), nil
}

// SerializeField is `impl SerializeStruct for SerializeInlineTable`.
func (s *serSerializeInlineTable) SerializeField(key string, value any) error {
	valueSerializer := mapValueSerializerNew()
	res, err := serde.Serialize(value, valueSerializer)
	if err == nil {
		kv := tableKeyValueNew(KeyNew(key), &Item_Value{F0: res.(Value)})
		s.items.insert(InternalString(key), kv)
	} else {
		e := serErrorFrom(err)
		if _, isNone := e.(*SerError_UnsupportedNone); !(isNone && valueSerializer.isNone) {
			return e
		}
	}
	return nil
}

// serDatetimeFieldSerializer is DatetimeFieldSerializer: what the string
// field of a datetime is serialized with. Its `Ok` is a Datetime; every
// compound method is `Impossible`.
type serDatetimeFieldSerializer struct{}

func (serDatetimeFieldSerializer) SerializeBool(bool) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeI8(int8) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeI16(int16) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeI32(int32) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeI64(int64) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeU8(uint8) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeU16(uint16) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeU32(uint32) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeU64(uint64) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeF32(float32) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeF64(float64) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeChar(rune) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeStr(v string) (any, error) {
	datetime, err := DatetimeFromStr(v)
	if err != nil {
		return nil, serErrorCustom(err.Error())
	}
	return datetime, nil
}

func (serDatetimeFieldSerializer) SerializeBytes([]byte) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeNone() (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeSome(any) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeUnit() (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeUnitStruct(string) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeUnitVariant(string, uint32, string) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeNewtypeStruct(string, any) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeNewtypeVariant(string, uint32, string, any) (any, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeTupleStruct(string, int) (serde.SerSerializeTupleStruct, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeTupleVariant(string, uint32, string, int) (serde.SerSerializeTupleVariant, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeStruct(string, int) (serde.SerSerializeStruct, error) {
	return nil, &SerError_DateInvalid{}
}

func (serDatetimeFieldSerializer) SerializeStructVariant(string, uint32, string, int) (serde.SerSerializeStructVariant, error) {
	return nil, &SerError_DateInvalid{}
}

// CollectStr is the default `Serializer::collect_str`.
func (s serDatetimeFieldSerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (serDatetimeFieldSerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

// mapValueSerializer is MapValueSerializer: ValueSerializer, noting whether
// the value it was handed was `None` itself.
type mapValueSerializer struct {
	isNone bool
}

func mapValueSerializerNew() *mapValueSerializer {
	return &mapValueSerializer{isNone: false}
}

func (s *mapValueSerializer) SerializeBool(v bool) (any, error) {
	return SerValueSerializerNew().SerializeBool(v)
}

func (s *mapValueSerializer) SerializeI8(v int8) (any, error) {
	return SerValueSerializerNew().SerializeI8(v)
}

func (s *mapValueSerializer) SerializeI16(v int16) (any, error) {
	return SerValueSerializerNew().SerializeI16(v)
}

func (s *mapValueSerializer) SerializeI32(v int32) (any, error) {
	return SerValueSerializerNew().SerializeI32(v)
}

func (s *mapValueSerializer) SerializeI64(v int64) (any, error) {
	return SerValueSerializerNew().SerializeI64(v)
}

func (s *mapValueSerializer) SerializeU8(v uint8) (any, error) {
	return SerValueSerializerNew().SerializeU8(v)
}

func (s *mapValueSerializer) SerializeU16(v uint16) (any, error) {
	return SerValueSerializerNew().SerializeU16(v)
}

func (s *mapValueSerializer) SerializeU32(v uint32) (any, error) {
	return SerValueSerializerNew().SerializeU32(v)
}

func (s *mapValueSerializer) SerializeU64(v uint64) (any, error) {
	return SerValueSerializerNew().SerializeU64(v)
}

func (s *mapValueSerializer) SerializeF32(v float32) (any, error) {
	return SerValueSerializerNew().SerializeF32(v)
}

func (s *mapValueSerializer) SerializeF64(v float64) (any, error) {
	return SerValueSerializerNew().SerializeF64(v)
}

func (s *mapValueSerializer) SerializeChar(v rune) (any, error) {
	return SerValueSerializerNew().SerializeChar(v)
}

func (s *mapValueSerializer) SerializeStr(v string) (any, error) {
	return SerValueSerializerNew().SerializeStr(v)
}

func (s *mapValueSerializer) SerializeBytes(value []byte) (any, error) {
	return SerValueSerializerNew().SerializeBytes(value)
}

func (s *mapValueSerializer) SerializeNone() (any, error) {
	s.isNone = true
	return nil, &SerError_UnsupportedNone{}
}

func (s *mapValueSerializer) SerializeSome(value any) (any, error) {
	return SerValueSerializerNew().SerializeSome(value)
}

func (s *mapValueSerializer) SerializeUnit() (any, error) {
	return SerValueSerializerNew().SerializeUnit()
}

func (s *mapValueSerializer) SerializeUnitStruct(name string) (any, error) {
	return SerValueSerializerNew().SerializeUnitStruct(name)
}

func (s *mapValueSerializer) SerializeUnitVariant(name string, variantIndex uint32, variant string) (any, error) {
	return SerValueSerializerNew().SerializeUnitVariant(name, variantIndex, variant)
}

func (s *mapValueSerializer) SerializeNewtypeStruct(name string, value any) (any, error) {
	return SerValueSerializerNew().SerializeNewtypeStruct(name, value)
}

func (s *mapValueSerializer) SerializeNewtypeVariant(name string, variantIndex uint32, variant string, value any) (any, error) {
	return SerValueSerializerNew().SerializeNewtypeVariant(name, variantIndex, variant, value)
}

func (s *mapValueSerializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	return SerValueSerializerNew().SerializeSeq(length)
}

func (s *mapValueSerializer) SerializeTuple(length int) (serde.SerSerializeTuple, error) {
	return SerValueSerializerNew().SerializeTuple(length)
}

func (s *mapValueSerializer) SerializeTupleStruct(name string, length int) (serde.SerSerializeTupleStruct, error) {
	return SerValueSerializerNew().SerializeTupleStruct(name, length)
}

func (s *mapValueSerializer) SerializeTupleVariant(name string, variantIndex uint32, variant string, length int) (serde.SerSerializeTupleVariant, error) {
	return SerValueSerializerNew().SerializeTupleVariant(name, variantIndex, variant, length)
}

func (s *mapValueSerializer) SerializeMap(length rs.Option[int]) (serde.SerSerializeMap, error) {
	return SerValueSerializerNew().SerializeMap(length)
}

func (s *mapValueSerializer) SerializeStruct(name string, length int) (serde.SerSerializeStruct, error) {
	return SerValueSerializerNew().SerializeStruct(name, length)
}

func (s *mapValueSerializer) SerializeStructVariant(name string, variantIndex uint32, variant string, length int) (serde.SerSerializeStructVariant, error) {
	return SerValueSerializerNew().SerializeStructVariant(name, variantIndex, variant, length)
}

// CollectStr is the default `Serializer::collect_str`.
func (s *mapValueSerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (s *mapValueSerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

// serSerializeTupleVariant is SerializeTupleVariant
// (`SerializeVariant<SerializeValueArray>`).
type serSerializeTupleVariant struct {
	variant string
	inner   *serSerializeValueArray
}

func serSerializeTupleVariantTuple(variant string, length int) *serSerializeTupleVariant {
	return &serSerializeTupleVariant{variant: variant, inner: serSerializeValueArrayWithCapacity(length)}
}

func (s *serSerializeTupleVariant) SerializeField(value any) error {
	return s.inner.SerializeElement(value)
}

func (s *serSerializeTupleVariant) End() (any, error) {
	inner, err := s.inner.End()
	if err != nil {
		return nil, err
	}
	var items keyValuePairs
	kv := tableKeyValueNew(KeyNew(s.variant), &Item_Value{F0: inner.(Value)})
	items.insert(InternalString(s.variant), kv)
	return ValueFromInlineTable(inlineTableWithPairs(items)), nil
}

// serSerializeStructVariant is SerializeStructVariant
// (`SerializeVariant<SerializeMap>`).
type serSerializeStructVariant struct {
	variant string
	inner   *serSerializeMap
}

func serSerializeStructVariantStruct(variant string, length int) *serSerializeStructVariant {
	return &serSerializeStructVariant{variant: variant, inner: serSerializeMapTableWithCapacity(length)}
}

func (s *serSerializeStructVariant) SerializeField(key string, value any) error {
	return s.inner.SerializeField(key, value)
}

// SkipField is the default `SerializeStructVariant::skip_field`.
func (s *serSerializeStructVariant) SkipField(string) error { return nil }

func (s *serSerializeStructVariant) End() (any, error) {
	inner, err := s.inner.End()
	if err != nil {
		return nil, err
	}
	var items keyValuePairs
	kv := tableKeyValueNew(KeyNew(s.variant), &Item_Value{F0: inner.(Value)})
	items.insert(InternalString(s.variant), kv)
	return ValueFromInlineTable(inlineTableWithPairs(items)), nil
}

var (
	_ serde.SerSerializeMap           = (*serSerializeMap)(nil)
	_ serde.SerSerializeStruct        = (*serSerializeMap)(nil)
	_ serde.Serializer                = serDatetimeFieldSerializer{}
	_ serde.Serializer                = (*mapValueSerializer)(nil)
	_ serde.SerSerializeTupleVariant  = (*serSerializeTupleVariant)(nil)
	_ serde.SerSerializeStructVariant = (*serSerializeStructVariant)(nil)
)
