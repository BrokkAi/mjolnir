// Port of toml_edit 0.20.2 src/de/mod.rs

package tomledit

import (
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// Deserializing TOML into Rust structures.
//
// This module contains all the Serde support for deserializing TOML documents
// into Rust structures.

// DeError is toml_edit::de::Error: errors that can occur when deserializing
// a type.
type DeError struct {
	inner TomlError
}

func deErrorCustom(msg string, span rs.Option[Range]) *DeError {
	return &DeError{inner: *tomlErrorCustom(msg, span)}
}

// deErrorFrom is how an error that comes back from a visitor or a seed is
// this format's error. In Rust the visitor makes a toml_edit::de::Error
// itself, through `serde::de::Error::custom` (which every other constructor
// of that trait ends in): an error with the message and no span. Here the
// visitor returns serde's own error, or any other, and this is where the
// format takes it. An error that already is a DeError passes unchanged.
func deErrorFrom(err error) *DeError {
	if own, ok := err.(*DeError); ok {
		return own
	}
	return deErrorCustom(err.Error(), rs.None[Range]())
}

// AddKey is Error::add_key: add key while unwinding.
func (e *DeError) AddKey(key string) {
	e.inner.addKey(key)
}

// Message is Error::message: what went wrong.
func (e *DeError) Message() string {
	return e.inner.Message()
}

// Span is Error::span: the start/end index into the original document where
// the error occurred.
func (e *DeError) Span() rs.Option[Range] {
	return e.inner.Span()
}

func (e *DeError) setSpan(span rs.Option[Range]) {
	e.inner.setSpan(span)
}

// spanIfNone is the closure Rust attaches to every result it passes back:
//
//	.map_err(|mut e: Self::Error| {
//	    if e.span().is_none() {
//	        e.set_span(span);
//	    }
//	    e
//	})
func spanIfNone(err error, span rs.Option[Range]) error {
	e := deErrorFrom(err)
	if e.Span().IsNone() {
		e.setSpan(span)
	}
	return e
}

// DeErrorCustom is `impl serde::de::Error for Error`: `Error::custom(msg)`.
func DeErrorCustom(msg string) *DeError {
	return deErrorCustom(msg, rs.None[Range]())
}

// Error is `impl Display for Error`.
func (e *DeError) Error() string {
	return e.inner.Error()
}

// FmtDisplay is `impl Display for Error`.
func (e *DeError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.Error()) }

// FmtDebug is the derived `Debug for Error`.
func (e *DeError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Error").Field("inner", &e.inner).Finish()
}

// Clone is the derived `Clone for Error`.
func (e *DeError) Clone() DeError {
	return DeError{inner: e.inner.Clone()}
}

// Equal is the derived `PartialEq for Error`.
func (e *DeError) Equal(other *DeError) bool {
	return e.inner.Equal(&other.inner)
}

// DeErrorFromTomlError is `impl From<TomlError> for Error`.
func DeErrorFromTomlError(e *TomlError) *DeError {
	return &DeError{inner: *e}
}

// TomlErrorFromDeError is `impl From<Error> for TomlError`.
func TomlErrorFromDeError(e *DeError) *TomlError {
	return &e.inner
}

// DeFromStr is toml_edit::de::from_str: convert a value into `T`.
func DeFromStr[T any](s string) (T, error) {
	de, err := DeDeserializerFromStr(s)
	if err != nil {
		var zero T
		return zero, err
	}
	return deserializeWith[T](de)
}

// deserializeWith is `T::deserialize(deserializer)`, with whatever error
// comes back taken as this format's.
func deserializeWith[T any](deserializer serde.Deserializer) (T, error) {
	value, err := serde.Deserialize[T](deserializer)
	if err != nil {
		var zero T
		return zero, deErrorFrom(err)
	}
	return value, nil
}

// DeFromSlice is toml_edit::de::from_slice: convert a value into `T`.
func DeFromSlice[T any](s []byte) (T, error) {
	text, err := strs.FromUtf8(s)
	if err != nil {
		var zero T
		return zero, deErrorCustom(err.Error(), rs.None[Range]())
	}
	return DeFromStr[T](text)
}

// DeFromDocument is toml_edit::de::from_document: convert a document into
// `T`.
func DeFromDocument[T any](d Document) (T, error) {
	deserializer := DeDeserializerNew(d)
	return deserializeWith[T](deserializer)
}

// DeDeserializer is toml_edit::de::Deserializer: deserialization for TOML
// documents.
type DeDeserializer struct {
	deForward
	input Document
}

// DeDeserializerNew is Deserializer::new: deserialization implementation for
// TOML.
func DeDeserializerNew(input Document) *DeDeserializer {
	d := &DeDeserializer{deForward: deForward{deserializeAny: nil}, input: input}
	d.deserializeAny = d.DeserializeAny
	return d
}

// DeDeserializerFromStr is `impl FromStr for Deserializer`: parses a
// document from a &str.
func DeDeserializerFromStr(s string) (*DeDeserializer, error) {
	d, err := parseDocument(s)
	if err != nil {
		return nil, DeErrorFromTomlError(err.(*TomlError))
	}
	return DeDeserializerNew(d), nil
}

// withOriginal is the closure the methods below attach to their result:
//
//	.map_err(|mut e: Self::Error| {
//	    e.inner.set_original(original);
//	    e
//	})
func (d *DeDeserializer) withOriginal(value any, err error) (any, error) {
	if err != nil {
		e := deErrorFrom(err)
		e.inner.setOriginal(d.input.original)
		return nil, e
	}
	return value, nil
}

// Note: this is wrapped by `toml::de::Deserializer` and any trait methods
// implemented here need to be wrapped there

// DeserializeAny is `Deserializer::deserialize_any`.
func (d *DeDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	return d.withOriginal(itemIntoDeserializer(d.input.root).DeserializeAny(visitor))
}

// DeserializeOption is `Deserializer::deserialize_option`.
//
// `None` is interpreted as a missing field so be sure to implement `Some`
// as a present field.
func (d *DeDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	return d.withOriginal(itemIntoDeserializer(d.input.root).DeserializeOption(visitor))
}

// DeserializeNewtypeStruct is `Deserializer::deserialize_newtype_struct`.
func (d *DeDeserializer) DeserializeNewtypeStruct(name string, visitor serde.DeVisitor) (any, error) {
	return d.withOriginal(itemIntoDeserializer(d.input.root).DeserializeNewtypeStruct(name, visitor))
}

// DeserializeStruct is `Deserializer::deserialize_struct`.
func (d *DeDeserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
	return d.withOriginal(itemIntoDeserializer(d.input.root).DeserializeStruct(name, fields, visitor))
}

// DeserializeEnum is `Deserializer::deserialize_enum`.
//
// Called when the type to deserialize is an enum, as opposed to a field in
// the type.
func (d *DeDeserializer) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	return d.withOriginal(itemIntoDeserializer(d.input.root).DeserializeEnum(name, variants, visitor))
}

// IntoDeserializer is `impl IntoDeserializer for Deserializer`: itself.
func (d *DeDeserializer) IntoDeserializer() *DeDeserializer {
	return d
}

// DocumentIntoDeserializer is `impl IntoDeserializer for Document`.
func DocumentIntoDeserializer(document Document) *DeDeserializer {
	return DeDeserializerNew(document)
}

func validateStructKeys(table *keyValuePairs, fields []string) error {
	var extraFields []*tableKeyValue
	for _, val := range table.entries {
		if !slices.Contains(fields, val.mapKey) {
			extraFields = append(extraFields, val)
		}
	}

	if len(extraFields) == 0 {
		return nil
	}
	keys := make([]string, len(extraFields))
	for index, k := range extraFields {
		keys[index] = k.key.Get()
	}
	return deErrorCustom(
		"unexpected keys in table: "+strings.Join(keys, ", ")+", available keys: "+strings.Join(fields, ", "),
		extraFields[0].key.span(),
	)
}

// The deserializers and accessors of this module, checked against the serde
// traits they implement.
var (
	_ serde.Deserializer    = (*DeDeserializer)(nil)
	_ serde.Deserializer    = (*DeValueDeserializer)(nil)
	_ serde.Deserializer    = (*arrayDeserializer)(nil)
	_ serde.Deserializer    = (*tableDeserializer)(nil)
	_ serde.Deserializer    = (*keyDeserializer)(nil)
	_ serde.Deserializer    = (*strDeserializer)(nil)
	_ serde.Deserializer    = (*borrowedStrDeserializer)(nil)
	_ serde.Deserializer    = (*stringDeserializer)(nil)
	_ serde.Deserializer    = (*usizeDeserializer)(nil)
	_ serde.DeSeqAccess     = (*arraySeqAccess)(nil)
	_ serde.DeMapAccess     = (*tableMapAccess)(nil)
	_ serde.DeMapAccess     = (*datetimeDeserializer)(nil)
	_ serde.DeMapAccess     = (*spannedDeserializer)(nil)
	_ serde.DeEnumAccess    = (*tableMapAccess)(nil)
	_ serde.DeEnumAccess    = (*keyDeserializer)(nil)
	_ serde.DeEnumAccess    = (*strDeserializer)(nil)
	_ serde.DeEnumAccess    = (*borrowedStrDeserializer)(nil)
	_ serde.DeEnumAccess    = (*stringDeserializer)(nil)
	_ serde.DeVariantAccess = unitOnly{}
	_ serde.DeVariantAccess = (*tableEnumDeserializer)(nil)
)
