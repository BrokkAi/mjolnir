// Port of serde_core 1.0.229 src/ser/mod.rs

package serde

import "github.com/BrokkAi/mjolnir/go/rs"

// Serializable is the Rust trait `Serialize` for a type whose impl is written
// by hand: the method holds the body of `fn serialize`. A type whose
// Serialize is derived needs no method; Serialize reads its registration.
//
// The method may have a value or a pointer receiver. An enum that is a Go
// interface cannot carry methods; its hand-written impl is registered with
// RegisterSerialize instead.
type Serializable interface {
	// SerializeSerde is `fn serialize<S: Serializer>(&self, serializer: S)
	// -> Result<S::Ok, S::Error>`.
	SerializeSerde(serializer Serializer) (any, error)
}

// Serializer is the Rust trait `serde::Serializer`: a data format that can
// serialize any data structure supported by serde.
//
// Every method that returns `Result<Self::Ok, Self::Error>` in Rust returns
// (any, error) here: the value is the format's `Ok` (nil for a format that
// writes to a stream, the built value for one that builds a tree).
//
// A value to serialize (`&T where T: Serialize`) is passed as `any`. It is a
// Go value of a serializable type or a pointer to one, and the format hands
// it straight back to Serialize together with the serializer that should
// receive it:
//
//	func (s *mySerializer) SerializeSome(value any) (any, error) {
//		return serde.Serialize(value, s)
//	}
type Serializer interface {
	// SerializeBool serializes a `bool`.
	SerializeBool(v bool) (any, error)
	// SerializeI8 serializes an `i8`.
	SerializeI8(v int8) (any, error)
	// SerializeI16 serializes an `i16`.
	SerializeI16(v int16) (any, error)
	// SerializeI32 serializes an `i32`.
	SerializeI32(v int32) (any, error)
	// SerializeI64 serializes an `i64`.
	SerializeI64(v int64) (any, error)
	// SerializeU8 serializes a `u8`.
	SerializeU8(v uint8) (any, error)
	// SerializeU16 serializes a `u16`.
	SerializeU16(v uint16) (any, error)
	// SerializeU32 serializes a `u32`.
	SerializeU32(v uint32) (any, error)
	// SerializeU64 serializes a `u64`.
	SerializeU64(v uint64) (any, error)
	// SerializeF32 serializes an `f32`.
	SerializeF32(v float32) (any, error)
	// SerializeF64 serializes an `f64`.
	SerializeF64(v float64) (any, error)
	// SerializeChar serializes a `char`.
	SerializeChar(v rune) (any, error)
	// SerializeStr serializes a `&str`.
	SerializeStr(v string) (any, error)
	// SerializeBytes serializes a chunk of raw byte data.
	SerializeBytes(v []byte) (any, error)
	// SerializeNone serializes a `None`.
	SerializeNone() (any, error)
	// SerializeSome serializes a `Some(T)`.
	SerializeSome(value any) (any, error)
	// SerializeUnit serializes a `()`.
	SerializeUnit() (any, error)
	// SerializeUnitStruct serializes a unit struct like `struct Unit`.
	SerializeUnitStruct(name string) (any, error)
	// SerializeUnitVariant serializes a unit variant like `E::A` in `enum E
	// { A, B }`. name is the enum, variantIndex the position of the variant
	// and variant its name.
	SerializeUnitVariant(name string, variantIndex uint32, variant string) (any, error)
	// SerializeNewtypeStruct serializes a newtype struct like `struct
	// Millimeters(u8)`.
	SerializeNewtypeStruct(name string, value any) (any, error)
	// SerializeNewtypeVariant serializes a newtype variant like `E::N` in
	// `enum E { N(u8) }`.
	SerializeNewtypeVariant(name string, variantIndex uint32, variant string, value any) (any, error)
	// SerializeSeq begins a variably sized sequence. len is the number of
	// elements when it is known.
	SerializeSeq(len rs.Option[int]) (SerSerializeSeq, error)
	// SerializeTuple begins a statically sized sequence.
	SerializeTuple(len int) (SerSerializeTuple, error)
	// SerializeTupleStruct begins a tuple struct like `struct Rgb(u8, u8,
	// u8)`.
	SerializeTupleStruct(name string, len int) (SerSerializeTupleStruct, error)
	// SerializeTupleVariant begins a tuple variant like `E::T` in `enum E {
	// T(u8, u8) }`.
	SerializeTupleVariant(name string, variantIndex uint32, variant string, len int) (SerSerializeTupleVariant, error)
	// SerializeMap begins a map. len is the number of entries when it is
	// known.
	SerializeMap(len rs.Option[int]) (SerSerializeMap, error)
	// SerializeStruct begins a struct like `struct Rgb { r: u8, g: u8, b: u8
	// }`. len is the number of fields that will be written.
	SerializeStruct(name string, len int) (SerSerializeStruct, error)
	// SerializeStructVariant begins a struct variant like `E::S` in `enum E {
	// S { r: u8, g: u8, b: u8 } }`.
	SerializeStructVariant(name string, variantIndex uint32, variant string, len int) (SerSerializeStructVariant, error)
	// CollectStr serializes a string produced by a Display impl. Rust's
	// `collect_str` takes the value and formats it; here the caller formats
	// first. A format with nothing better to do calls
	// SerializerDefaultCollectStr.
	CollectStr(value string) (any, error)
	// IsHumanReadable reports whether Serialize impls should write their
	// human-readable form. A format with nothing to say calls
	// SerializerDefaultIsHumanReadable, which is true.
	IsHumanReadable() bool
}

// SerSerializer is `serde::ser::Serializer`, the path some Rust code writes
// for Serializer.
type SerSerializer = Serializer

// SerializerDefaultCollectStr is the default body of `Serializer::collect_str`.
func SerializerDefaultCollectStr(serializer Serializer, value string) (any, error) {
	return serializer.SerializeStr(value)
}

// SerializerDefaultIsHumanReadable is the default body of
// `Serializer::is_human_readable`.
func SerializerDefaultIsHumanReadable() bool { return true }

// SerSerializeSeq is `ser::SerializeSeq`, returned by SerializeSeq.
type SerSerializeSeq interface {
	// SerializeElement serializes a sequence element.
	SerializeElement(value any) error
	// End finishes serializing the sequence.
	End() (any, error)
}

// SerSerializeTuple is `ser::SerializeTuple`, returned by SerializeTuple.
type SerSerializeTuple interface {
	// SerializeElement serializes a tuple element.
	SerializeElement(value any) error
	// End finishes serializing the tuple.
	End() (any, error)
}

// SerSerializeTupleStruct is `ser::SerializeTupleStruct`, returned by
// SerializeTupleStruct.
type SerSerializeTupleStruct interface {
	// SerializeField serializes a tuple struct field.
	SerializeField(value any) error
	// End finishes serializing the tuple struct.
	End() (any, error)
}

// SerSerializeTupleVariant is `ser::SerializeTupleVariant`, returned by
// SerializeTupleVariant.
type SerSerializeTupleVariant interface {
	// SerializeField serializes a tuple variant field.
	SerializeField(value any) error
	// End finishes serializing the tuple variant.
	End() (any, error)
}

// SerSerializeMap is `ser::SerializeMap`, returned by SerializeMap.
type SerSerializeMap interface {
	// SerializeKey serializes a map key.
	SerializeKey(key any) error
	// SerializeValue serializes a map value. It follows a SerializeKey.
	SerializeValue(value any) error
	// SerializeEntry serializes a key and its value. An implementation with
	// nothing better to do calls SerSerializeMapDefaultSerializeEntry.
	SerializeEntry(key any, value any) error
	// End finishes serializing the map.
	End() (any, error)
}

// SerSerializeMapDefaultSerializeEntry is the default body of
// `SerializeMap::serialize_entry`.
func SerSerializeMapDefaultSerializeEntry(m SerSerializeMap, key any, value any) error {
	if err := m.SerializeKey(key); err != nil {
		return err
	}
	return m.SerializeValue(value)
}

// SerSerializeStruct is `ser::SerializeStruct`, returned by SerializeStruct.
type SerSerializeStruct interface {
	// SerializeField serializes a struct field.
	SerializeField(key string, value any) error
	// SkipField says that a struct field was skipped. An implementation with
	// nothing to do returns nil, as the Rust default does.
	SkipField(key string) error
	// End finishes serializing the struct.
	End() (any, error)
}

// SerSerializeStructVariant is `ser::SerializeStructVariant`, returned by
// SerializeStructVariant.
type SerSerializeStructVariant interface {
	// SerializeField serializes a struct variant field.
	SerializeField(key string, value any) error
	// SkipField says that a struct variant field was skipped. An
	// implementation with nothing to do returns nil, as the Rust default
	// does.
	SkipField(key string) error
	// End finishes serializing the struct variant.
	End() (any, error)
}
