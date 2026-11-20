// Port of toml_edit 0.20.2 src/ser/key.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// serKeySerializer is KeySerializer: what a map key is serialized with. Its
// `Ok` is an InternalString; every compound method is `Impossible`.
type serKeySerializer struct{}

func (serKeySerializer) SerializeBool(bool) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeI8(int8) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeI16(int16) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeI32(int32) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeI64(int64) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeU8(uint8) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeU16(uint16) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeU32(uint32) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeU64(uint64) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeF32(float32) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeF64(float64) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeChar(rune) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeStr(value string) (any, error) {
	return InternalString(value), nil
}

func (serKeySerializer) SerializeBytes([]byte) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeNone() (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeSome(any) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeUnit() (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeUnitStruct(string) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return InternalString(variant), nil
}

func (s serKeySerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	key, err := serde.Serialize(value, s)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return key, nil
}

func (serKeySerializer) SerializeNewtypeVariant(string, uint32, string, any) (any, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeTupleStruct(string, int) (serde.SerSerializeTupleStruct, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeTupleVariant(string, uint32, string, int) (serde.SerSerializeTupleVariant, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeStruct(string, int) (serde.SerSerializeStruct, error) {
	return nil, &SerError_KeyNotString{}
}

func (serKeySerializer) SerializeStructVariant(string, uint32, string, int) (serde.SerSerializeStructVariant, error) {
	return nil, &SerError_KeyNotString{}
}

// CollectStr is the default `Serializer::collect_str`.
func (s serKeySerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (serKeySerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

var _ serde.Serializer = serKeySerializer{}
