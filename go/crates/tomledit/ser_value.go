// Port of toml_edit 0.20.2 src/ser/value.rs

package tomledit

import (
	"math"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// SerValueSerializer is toml_edit::ser::ValueSerializer: serialization for
// TOML values.
//
// This structure implements serialization support for TOML to serialize an
// arbitrary type to TOML. Note that the TOML format does not support all
// datatypes in Rust, such as enums, tuples, and tuple structs. These types
// will generate an error when serialized.
//
// Currently a serializer always writes its output to an in-memory `String`,
// which is passed in when creating the serializer itself.
//
// Its `Ok` (the value the methods return) is a Value.
//
//	#[derive(Serialize)]
//	struct Config {
//	    database: Database,
//	}
//
//	#[derive(Serialize)]
//	struct Database {
//	    ip: String,
//	    port: Vec<u16>,
//	    connection_max: u32,
//	    enabled: bool,
//	}
//
//	let config = Config {
//	    database: Database {
//	        ip: "192.168.1.1".to_string(),
//	        port: vec![8001, 8002, 8003],
//	        connection_max: 5000,
//	        enabled: false,
//	    },
//	};
//
//	let value = serde::Serialize::serialize(
//	    &config,
//	    toml_edit::ser::ValueSerializer::new()
//	).unwrap();
//	println!("{}", value)
type SerValueSerializer struct{}

// SerValueSerializerNew is ValueSerializer::new: creates a new serializer
// generate a TOML document.
func SerValueSerializerNew() *SerValueSerializer {
	return &SerValueSerializer{}
}

// SerValueSerializerDefault is the derived `Default for ValueSerializer`.
func SerValueSerializerDefault() *SerValueSerializer {
	return &SerValueSerializer{}
}

func (s *SerValueSerializer) SerializeBool(v bool) (any, error) {
	return ValueFromBool(v), nil
}

func (s *SerValueSerializer) SerializeI8(v int8) (any, error) {
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeI16(v int16) (any, error) {
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeI32(v int32) (any, error) {
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeI64(v int64) (any, error) {
	return ValueFromInt64(v), nil
}

func (s *SerValueSerializer) SerializeU8(v uint8) (any, error) {
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeU16(v uint16) (any, error) {
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeU32(v uint32) (any, error) {
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeU64(v uint64) (any, error) {
	if v > math.MaxInt64 {
		return nil, &SerError_OutOfRange{F0: rs.Some("u64")}
	}
	return s.SerializeI64(int64(v))
}

func (s *SerValueSerializer) SerializeF32(v float32) (any, error) {
	return s.SerializeF64(float64(v))
}

func (s *SerValueSerializer) SerializeF64(v float64) (any, error) {
	return ValueFromFloat64(v), nil
}

func (s *SerValueSerializer) SerializeChar(v rune) (any, error) {
	return s.SerializeStr(string(v))
}

func (s *SerValueSerializer) SerializeStr(v string) (any, error) {
	return ValueFromString(v), nil
}

// SerializeBytes is `value.serialize(self)` on the `&[u8]`: the bytes as a
// sequence of u8, which `impl Serialize for [T]` writes with collect_seq.
func (s *SerValueSerializer) SerializeBytes(value []byte) (any, error) {
	return serializeBytesAsSeq(s, value)
}

// serializeBytesAsSeq is `impl Serialize for [u8]` (`serializer.collect_seq`):
// a sequence of the length, each byte an element.
func serializeBytesAsSeq(serializer serde.Serializer, value []byte) (any, error) {
	seq, err := serializer.SerializeSeq(rs.Some(len(value)))
	if err != nil {
		return nil, err
	}
	for _, b := range value {
		if err := seq.SerializeElement(b); err != nil {
			return nil, err
		}
	}
	return seq.End()
}

func (s *SerValueSerializer) SerializeNone() (any, error) {
	return nil, &SerError_UnsupportedNone{}
}

func (s *SerValueSerializer) SerializeSome(value any) (any, error) {
	return serializeValue(value, s)
}

func (s *SerValueSerializer) SerializeUnit() (any, error) {
	return nil, &SerError_UnsupportedType{F0: rs.Some("unit")}
}

func (s *SerValueSerializer) SerializeUnitStruct(name string) (any, error) {
	return nil, &SerError_UnsupportedType{F0: rs.Some(name)}
}

func (s *SerValueSerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return s.SerializeStr(variant)
}

func (s *SerValueSerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return serializeValue(value, s)
}

func (s *SerValueSerializer) SerializeNewtypeVariant(_ string, _ uint32, variant string, value any) (any, error) {
	serialized, err := serializeValue(value, s)
	if err != nil {
		return nil, err
	}
	table := InlineTableNew()
	table.Insert(variant, serialized)
	return ValueFromInlineTable(table), nil
}

func (s *SerValueSerializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	var serializer *serSerializeValueArray
	if n, ok := length.Get(); ok {
		serializer = serSerializeValueArrayWithCapacity(n)
	} else {
		serializer = serSerializeValueArrayNew()
	}
	return serializer, nil
}

func (s *SerValueSerializer) SerializeTuple(length int) (serde.SerSerializeTuple, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*serSerializeValueArray), nil
}

func (s *SerValueSerializer) SerializeTupleStruct(_ string, length int) (serde.SerSerializeTupleStruct, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*serSerializeValueArray), nil
}

func (s *SerValueSerializer) SerializeTupleVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeTupleVariant, error) {
	return serSerializeTupleVariantTuple(variant, length), nil
}

func (s *SerValueSerializer) SerializeMap(length rs.Option[int]) (serde.SerSerializeMap, error) {
	var serializer *serSerializeMap
	if n, ok := length.Get(); ok {
		serializer = serSerializeMapTableWithCapacity(n)
	} else {
		serializer = serSerializeMapTable()
	}
	return serializer, nil
}

func (s *SerValueSerializer) SerializeStruct(name string, length int) (serde.SerSerializeStruct, error) {
	if name == DatetimeName {
		return serSerializeMapDatetime(), nil
	}
	serializer, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return serializer.(*serSerializeMap), nil
}

func (s *SerValueSerializer) SerializeStructVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeStructVariant, error) {
	return serSerializeStructVariantStruct(variant, length), nil
}

// CollectStr is the default `Serializer::collect_str`.
func (s *SerValueSerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (s *SerValueSerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

var _ serde.Serializer = (*SerValueSerializer)(nil)
