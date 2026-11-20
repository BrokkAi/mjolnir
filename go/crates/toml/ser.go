// Port of toml 0.8.2 src/ser.rs

package toml

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Serializing Rust structures into TOML.
//
// This module contains all the Serde support for serializing Rust structures
// into TOML documents (as strings). Note that some top-level functions here
// are also provided at the top of the crate.

// ToString is toml::to_string (toml::ser::to_string): serialize the given
// data structure as a String of TOML.
//
// Serialization can fail if `T`'s implementation of `Serialize` decides to
// fail, if `T` contains a map with non-string keys, or if `T` attempts to
// serialize an unsupported datatype such as an enum, tuple, or tuple struct.
//
// To serialize TOML values, instead of documents, see `ValueSerializer`.
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
//	let toml = toml::to_string(&config).unwrap();
//	println!("{}", toml)
func ToString(value any) (string, error) {
	output := ""
	serializer := SerializerNew(&output)
	if _, err := serde.Serialize(value, serializer); err != nil {
		return "", serErrorFrom(err)
	}
	return output, nil
}

// ToStringPretty is toml::to_string_pretty (toml::ser::to_string_pretty):
// serialize the given data structure as a "pretty" String of TOML.
//
// This is identical to `to_string` except the output string has a more
// "pretty" output. See `Serializer::pretty` for more details.
//
// To serialize TOML values, instead of documents, see `ValueSerializer`.
//
// For greater customization, instead serialize to a
// [`toml_edit::Document`](https://docs.rs/toml_edit/latest/toml_edit/struct.Document.html).
func ToStringPretty(value any) (string, error) {
	output := ""
	serializer := SerializerPretty(&output)
	if _, err := serde.Serialize(value, serializer); err != nil {
		return "", serErrorFrom(err)
	}
	return output, nil
}

// SerError is toml::ser::Error: errors that can occur when serializing a
// type.
type SerError struct {
	inner tomledit.SerError
}

var _ = rs.RegisterStruct[SerError]("Error")

func serErrorNew(inner string) *SerError {
	return &SerError{inner: &tomledit.SerError_Custom{F0: inner}}
}

func serErrorWrap(inner tomledit.SerError) *SerError {
	return &SerError{inner: inner}
}

func serErrorUnsupportedType(t rs.Option[string]) *SerError {
	return &SerError{inner: &tomledit.SerError_UnsupportedType{F0: t}}
}

func serErrorUnsupportedNone() *SerError {
	return &SerError{inner: &tomledit.SerError_UnsupportedNone{}}
}

func serErrorKeyNotString() *SerError {
	return &SerError{inner: &tomledit.SerError_KeyNotString{}}
}

// isUnsupportedNone is the pattern `Error { inner:
// toml_edit::ser::Error::UnsupportedNone }`.
func (e *SerError) isUnsupportedNone() bool {
	_, ok := e.inner.(*tomledit.SerError_UnsupportedNone)
	return ok
}

// serErrorFrom is how an error that comes back from a Serialize impl, or
// from the toml_edit serializer this one wraps, is this format's error. In
// Rust a Serialize impl makes a toml::ser::Error itself, through
// `serde::ser::Error::custom`: here it returns serde's own error, or any
// other, and this is where the format takes it, as `Error::new` with the
// message. A toml_edit error is wrapped as `Error::wrap` wraps it, and a
// toml::ser::Error passes unchanged.
func serErrorFrom(err error) *SerError {
	switch e := err.(type) {
	case *SerError:
		return e
	case tomledit.SerError:
		return serErrorWrap(e)
	}
	return serErrorNew(err.Error())
}

// SerErrorCustom is `impl serde::ser::Error for Error`: `Error::custom(msg)`.
func SerErrorCustom(msg string) *SerError {
	return serErrorNew(msg)
}

// Error is `impl Display for Error`.
func (e *SerError) Error() string {
	return e.inner.Error()
}

// String is `impl Display for Error`.
func (e *SerError) String() string {
	return e.inner.Error()
}

// FmtDisplay is `impl Display for Error`, which delegates to the inner
// error's.
func (e *SerError) FmtDisplay(f *rs.Formatter) error {
	return e.inner.FmtDisplay(f)
}

// FmtDebug is the derived `Debug for Error`.
func (e *SerError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Error").Field("inner", e.inner).Finish()
}

// Clone is the derived `Clone for Error`.
func (e *SerError) Clone() SerError {
	return SerError{inner: rs.Clone(e.inner)}
}

// Equal is the derived `PartialEq for Error`.
func (e *SerError) Equal(other *SerError) bool {
	return rs.Eq(e.inner, other.inner)
}

// Serializer is toml::Serializer (toml::ser::Serializer): serialization for
// TOML documents.
//
// This structure implements serialization support for TOML to serialize an
// arbitrary type to TOML. Note that the TOML format does not support all
// datatypes in Rust, such as enums, tuples, and tuple structs. These types
// will generate an error when serialized.
//
// Currently a serializer always writes its output to an in-memory `String`,
// which is passed in when creating the serializer itself.
//
// To serialize TOML values, instead of documents, see `ValueSerializer`.
//
// Its `Ok` is `()`: the methods return nil.
type Serializer struct {
	dst      *string
	settings documentFormatter
}

// SerializerNew is Serializer::new: creates a new serializer which will emit
// TOML into the buffer provided.
//
// The serializer can then be used to serialize a type after which the data
// will be present in `dst`.
func SerializerNew(dst *string) *Serializer {
	return &Serializer{dst: dst, settings: documentFormatterDefault()}
}

// SerializerPretty is Serializer::pretty: apply a default "pretty" policy to
// the document.
//
// For greater customization, instead serialize to a
// [`toml_edit::Document`](https://docs.rs/toml_edit/latest/toml_edit/struct.Document.html).
func SerializerPretty(dst *string) *Serializer {
	ser := SerializerNew(dst)
	ser.settings.multilineArray = true
	return ser
}

// editResult is the `Result<toml_edit::Value, toml_edit::ser::Error>` a
// toml_edit serializer method returns.
type editResult struct {
	value tomledit.Value
	err   error
}

// editValue packs what a toml_edit serializer method returned.
func editValue(value any, err error) editResult {
	if err != nil {
		return editResult{value: nil, err: err}
	}
	return editResult{value: value.(tomledit.Value), err: nil}
}

func (s *Serializer) SerializeBool(v bool) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeBool(v)))
}

func (s *Serializer) SerializeI8(v int8) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeI8(v)))
}

func (s *Serializer) SerializeI16(v int16) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeI16(v)))
}

func (s *Serializer) SerializeI32(v int32) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeI32(v)))
}

func (s *Serializer) SerializeI64(v int64) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeI64(v)))
}

func (s *Serializer) SerializeU8(v uint8) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeU8(v)))
}

func (s *Serializer) SerializeU16(v uint16) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeU16(v)))
}

func (s *Serializer) SerializeU32(v uint32) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeU32(v)))
}

func (s *Serializer) SerializeU64(v uint64) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeU64(v)))
}

func (s *Serializer) SerializeF32(v float32) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeF32(v)))
}

func (s *Serializer) SerializeF64(v float64) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeF64(v)))
}

func (s *Serializer) SerializeChar(v rune) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeChar(v)))
}

func (s *Serializer) SerializeStr(v string) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeStr(v)))
}

func (s *Serializer) SerializeBytes(v []byte) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeBytes(v)))
}

func (s *Serializer) SerializeNone() (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeNone()))
}

func (s *Serializer) SerializeSome(v any) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeSome(v)))
}

func (s *Serializer) SerializeUnit() (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeUnit()))
}

func (s *Serializer) SerializeUnitStruct(name string) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeUnitStruct(name)))
}

func (s *Serializer) SerializeUnitVariant(name string, variantIndex uint32, variant string) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(
		tomledit.SerValueSerializerNew().SerializeUnitVariant(name, variantIndex, variant),
	))
}

func (s *Serializer) SerializeNewtypeStruct(name string, v any) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(tomledit.SerValueSerializerNew().SerializeNewtypeStruct(name, v)))
}

func (s *Serializer) SerializeNewtypeVariant(name string, variantIndex uint32, variant string, value any) (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(
		tomledit.SerValueSerializerNew().SerializeNewtypeVariant(name, variantIndex, variant, value),
	))
}

func (s *Serializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	ser, err := tomledit.SerValueSerializerNew().SerializeSeq(length)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serSerializeDocumentArrayNew(s, ser), nil
}

func (s *Serializer) SerializeTuple(length int) (serde.SerSerializeTuple, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*serSerializeDocumentArray), nil
}

func (s *Serializer) SerializeTupleStruct(_ string, length int) (serde.SerSerializeTupleStruct, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*serSerializeDocumentArray), nil
}

func (s *Serializer) SerializeTupleVariant(_ string, _ uint32, _ string, length int) (serde.SerSerializeTupleVariant, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*serSerializeDocumentArray), nil
}

func (s *Serializer) SerializeMap(length rs.Option[int]) (serde.SerSerializeMap, error) {
	ser, err := tomledit.SerValueSerializerNew().SerializeMap(length)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serSerializeDocumentTableNew(s, ser), nil
}

func (s *Serializer) SerializeStruct(_ string, length int) (serde.SerSerializeStruct, error) {
	m, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return m.(*serSerializeDocumentTable), nil
}

func (s *Serializer) SerializeStructVariant(name string, _ uint32, _ string, _ int) (serde.SerSerializeStructVariant, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

// CollectStr is the default `Serializer::collect_str`.
func (s *Serializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (s *Serializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

// SerValueSerializer is toml::ser::ValueSerializer: serialization for TOML
// values.
//
// This structure implements serialization support for TOML to serialize an
// arbitrary type to TOML. Note that the TOML format does not support all
// datatypes in Rust, such as enums, tuples, and tuple structs. These types
// will generate an error when serialized.
//
// Currently a serializer always writes its output to an in-memory `String`,
// which is passed in when creating the serializer itself.
//
// Its `Ok` is `()`: the methods return nil.
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
//	let mut value = String::new();
//	serde::Serialize::serialize(
//	    &config,
//	    toml::ser::ValueSerializer::new(&mut value)
//	).unwrap();
//	println!("{}", value)
type SerValueSerializer struct {
	dst *string
}

// SerValueSerializerNew is ValueSerializer::new: creates a new serializer
// which will emit TOML into the buffer provided.
//
// The serializer can then be used to serialize a type after which the data
// will be present in `dst`.
func SerValueSerializerNew(dst *string) *SerValueSerializer {
	return &SerValueSerializer{dst: dst}
}

func (s *SerValueSerializer) SerializeBool(v bool) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeBool(v)))
}

func (s *SerValueSerializer) SerializeI8(v int8) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeI8(v)))
}

func (s *SerValueSerializer) SerializeI16(v int16) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeI16(v)))
}

func (s *SerValueSerializer) SerializeI32(v int32) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeI32(v)))
}

func (s *SerValueSerializer) SerializeI64(v int64) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeI64(v)))
}

func (s *SerValueSerializer) SerializeU8(v uint8) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeU8(v)))
}

func (s *SerValueSerializer) SerializeU16(v uint16) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeU16(v)))
}

func (s *SerValueSerializer) SerializeU32(v uint32) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeU32(v)))
}

func (s *SerValueSerializer) SerializeU64(v uint64) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeU64(v)))
}

func (s *SerValueSerializer) SerializeF32(v float32) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeF32(v)))
}

func (s *SerValueSerializer) SerializeF64(v float64) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeF64(v)))
}

func (s *SerValueSerializer) SerializeChar(v rune) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeChar(v)))
}

func (s *SerValueSerializer) SerializeStr(v string) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeStr(v)))
}

func (s *SerValueSerializer) SerializeBytes(v []byte) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeBytes(v)))
}

func (s *SerValueSerializer) SerializeNone() (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeNone()))
}

func (s *SerValueSerializer) SerializeSome(v any) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeSome(v)))
}

func (s *SerValueSerializer) SerializeUnit() (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeUnit()))
}

func (s *SerValueSerializer) SerializeUnitStruct(name string) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeUnitStruct(name)))
}

func (s *SerValueSerializer) SerializeUnitVariant(name string, variantIndex uint32, variant string) (any, error) {
	return nil, writeValue(s.dst, editValue(
		tomledit.SerValueSerializerNew().SerializeUnitVariant(name, variantIndex, variant),
	))
}

func (s *SerValueSerializer) SerializeNewtypeStruct(name string, v any) (any, error) {
	return nil, writeValue(s.dst, editValue(tomledit.SerValueSerializerNew().SerializeNewtypeStruct(name, v)))
}

func (s *SerValueSerializer) SerializeNewtypeVariant(name string, variantIndex uint32, variant string, value any) (any, error) {
	return nil, writeValue(s.dst, editValue(
		tomledit.SerValueSerializerNew().SerializeNewtypeVariant(name, variantIndex, variant, value),
	))
}

func (s *SerValueSerializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	ser, err := tomledit.SerValueSerializerNew().SerializeSeq(length)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serSerializeValueArrayNew(s, ser), nil
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

func (s *SerValueSerializer) SerializeTupleVariant(_ string, _ uint32, _ string, length int) (serde.SerSerializeTupleVariant, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*serSerializeValueArray), nil
}

func (s *SerValueSerializer) SerializeMap(length rs.Option[int]) (serde.SerSerializeMap, error) {
	ser, err := tomledit.SerValueSerializerNew().SerializeMap(length)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serSerializeValueTableNew(s, ser), nil
}

func (s *SerValueSerializer) SerializeStruct(_ string, length int) (serde.SerSerializeStruct, error) {
	m, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return m.(*serSerializeValueTable), nil
}

func (s *SerValueSerializer) SerializeStructVariant(name string, _ uint32, _ string, _ int) (serde.SerSerializeStructVariant, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

// CollectStr is the default `Serializer::collect_str`.
func (s *SerValueSerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (s *SerValueSerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

// The serializers of ser.rs's `internal` module.

// structFieldSerializer is the SerializeStruct side of toml_edit's
// SerializeMap, which the SerializeMap the toml_edit serializer returns also
// is.
type structFieldSerializer interface {
	SerializeField(key string, value any) error
}

// serSerializeDocumentArray is SerializeDocumentArray.
type serSerializeDocumentArray struct {
	inner    serde.SerSerializeSeq
	dst      *string
	settings documentFormatter
}

func serSerializeDocumentArrayNew(ser *Serializer, inner serde.SerSerializeSeq) *serSerializeDocumentArray {
	return &serSerializeDocumentArray{inner: inner, dst: ser.dst, settings: ser.settings}
}

// SerializeElement is `SerializeSeq::serialize_element` (and
// `SerializeTuple::serialize_element`).
func (s *serSerializeDocumentArray) SerializeElement(value any) error {
	if err := s.inner.SerializeElement(value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

// SerializeField is `SerializeTupleVariant::serialize_field` and
// `SerializeTupleStruct::serialize_field`, which toml_edit's array
// serializer implements as serialize_element.
func (s *serSerializeDocumentArray) SerializeField(value any) error {
	if err := s.inner.SerializeElement(value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

func (s *serSerializeDocumentArray) End() (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(s.inner.End()))
}

// serSerializeDocumentTable is SerializeDocumentTable.
type serSerializeDocumentTable struct {
	inner    serde.SerSerializeMap
	dst      *string
	settings documentFormatter
}

func serSerializeDocumentTableNew(ser *Serializer, inner serde.SerSerializeMap) *serSerializeDocumentTable {
	return &serSerializeDocumentTable{inner: inner, dst: ser.dst, settings: ser.settings}
}

func (s *serSerializeDocumentTable) SerializeKey(input any) error {
	if err := s.inner.SerializeKey(input); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

func (s *serSerializeDocumentTable) SerializeValue(value any) error {
	if err := s.inner.SerializeValue(value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

// SerializeEntry is the default `SerializeMap::serialize_entry`.
func (s *serSerializeDocumentTable) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(s, key, value)
}

func (s *serSerializeDocumentTable) End() (any, error) {
	return nil, writeDocument(s.dst, s.settings, editValue(s.inner.End()))
}

// SerializeField is `impl SerializeStruct for SerializeDocumentTable`.
func (s *serSerializeDocumentTable) SerializeField(key string, value any) error {
	if err := s.inner.(structFieldSerializer).SerializeField(key, value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

// SkipField is the default `SerializeStruct::skip_field`.
func (s *serSerializeDocumentTable) SkipField(string) error { return nil }

func writeDocument(dst *string, settings documentFormatter, result editResult) error {
	if result.err != nil {
		return serErrorFrom(result.err)
	}
	var item tomledit.Item = &tomledit.Item_Value{F0: result.value}
	table, err := item.IntoTable()
	if err != nil {
		return serErrorUnsupportedType(rs.None[string]())
	}

	settings.VisitTableMut(&table)

	doc := tomledit.DocumentFromTable(table)
	*dst += doc.String()

	return nil
}

// serSerializeValueArray is SerializeValueArray.
type serSerializeValueArray struct {
	inner serde.SerSerializeSeq
	dst   *string
}

func serSerializeValueArrayNew(ser *SerValueSerializer, inner serde.SerSerializeSeq) *serSerializeValueArray {
	return &serSerializeValueArray{inner: inner, dst: ser.dst}
}

// SerializeElement is `SerializeSeq::serialize_element` (and
// `SerializeTuple::serialize_element`).
func (s *serSerializeValueArray) SerializeElement(value any) error {
	if err := s.inner.SerializeElement(value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

// SerializeField is `SerializeTupleVariant::serialize_field` and
// `SerializeTupleStruct::serialize_field`, which toml_edit's array
// serializer implements as serialize_element.
func (s *serSerializeValueArray) SerializeField(value any) error {
	if err := s.inner.SerializeElement(value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

func (s *serSerializeValueArray) End() (any, error) {
	return nil, writeValue(s.dst, editValue(s.inner.End()))
}

// serSerializeValueTable is SerializeValueTable.
type serSerializeValueTable struct {
	inner serde.SerSerializeMap
	dst   *string
}

func serSerializeValueTableNew(ser *SerValueSerializer, inner serde.SerSerializeMap) *serSerializeValueTable {
	return &serSerializeValueTable{inner: inner, dst: ser.dst}
}

func (s *serSerializeValueTable) SerializeKey(input any) error {
	if err := s.inner.SerializeKey(input); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

func (s *serSerializeValueTable) SerializeValue(value any) error {
	if err := s.inner.SerializeValue(value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

// SerializeEntry is the default `SerializeMap::serialize_entry`.
func (s *serSerializeValueTable) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(s, key, value)
}

func (s *serSerializeValueTable) End() (any, error) {
	return nil, writeValue(s.dst, editValue(s.inner.End()))
}

// SerializeField is `impl SerializeStruct for SerializeValueTable`.
func (s *serSerializeValueTable) SerializeField(key string, value any) error {
	if err := s.inner.(structFieldSerializer).SerializeField(key, value); err != nil {
		return serErrorFrom(err)
	}
	return nil
}

// SkipField is the default `SerializeStruct::skip_field`.
func (s *serSerializeValueTable) SkipField(string) error { return nil }

func writeValue(dst *string, result editResult) error {
	if result.err != nil {
		return serErrorFrom(result.err)
	}

	*dst += result.value.String()

	return nil
}

var (
	_ serde.Serializer               = (*Serializer)(nil)
	_ serde.Serializer               = (*SerValueSerializer)(nil)
	_ serde.SerSerializeSeq          = (*serSerializeDocumentArray)(nil)
	_ serde.SerSerializeTupleVariant = (*serSerializeDocumentArray)(nil)
	_ serde.SerSerializeMap          = (*serSerializeDocumentTable)(nil)
	_ serde.SerSerializeStruct       = (*serSerializeDocumentTable)(nil)
	_ serde.SerSerializeSeq          = (*serSerializeValueArray)(nil)
	_ serde.SerSerializeTupleVariant = (*serSerializeValueArray)(nil)
	_ serde.SerSerializeMap          = (*serSerializeValueTable)(nil)
	_ serde.SerSerializeStruct       = (*serSerializeValueTable)(nil)
)
