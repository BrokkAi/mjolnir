// Port of toml 0.8.2 src/de.rs

package toml

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Deserializing TOML into Rust structures.
//
// This module contains all the Serde support for deserializing TOML documents
// into Rust structures. Note that some top-level functions here are also
// provided at the top of the crate.

// FromStr is toml::from_str (toml::de::from_str): deserializes a string into
// a type.
//
// This function will attempt to interpret `s` as a TOML document and
// deserialize `T` from the document.
//
// To deserializes TOML values, instead of documents, see `ValueDeserializer`.
//
//	#[derive(Deserialize)]
//	struct Config {
//	    title: String,
//	    owner: Owner,
//	}
//
//	#[derive(Deserialize)]
//	struct Owner {
//	    name: String,
//	}
//
//	let config: Config = toml::from_str(r#"
//	    title = 'TOML Example'
//
//	    [owner]
//	    name = 'Lisa'
//	"#).unwrap();
//
//	assert_eq!(config.title, "TOML Example");
//	assert_eq!(config.owner.name, "Lisa");
func FromStr[T any](s string) (T, error) {
	return deserializeWith[T](DeserializerNew(s))
}

// deserializeWith is `T::deserialize(deserializer)`, with whatever error
// comes back taken as this format's: an error a Deserialize impl makes after
// the deserializer returned (a `try_from` conversion, say) reaches the entry
// point without passing through the format.
func deserializeWith[T any](deserializer serde.Deserializer) (T, error) {
	value, err := serde.Deserialize[T](deserializer)
	if err != nil {
		var zero T
		return zero, deErrorFrom(err)
	}
	return value, nil
}

// DeError is toml::de::Error: errors that can occur when deserializing a
// type.
type DeError struct {
	inner tomledit.DeError
}

var _ = rs.RegisterStruct[DeError]("Error")

func deErrorNew(inner *tomledit.DeError) *DeError {
	return &DeError{inner: *inner}
}

// deErrorFrom is how an error that comes back from a visitor or a seed is
// this format's error. In Rust the visitor makes a toml::de::Error itself,
// through `serde::de::Error::custom` (which every other constructor of that
// trait ends in): an error with the message and no span. Here the visitor
// returns serde's own error, or any other, and this is where the format
// takes it. An error that already is a toml::de::Error passes unchanged, and
// a toml_edit one is wrapped as `Error::new` wraps it.
func deErrorFrom(err error) *DeError {
	switch e := err.(type) {
	case *DeError:
		return e
	case *tomledit.DeError:
		return deErrorNew(e)
	}
	return DeErrorCustom(err.Error())
}

func (e *DeError) addKey(key string) {
	e.inner.AddKey(key)
}

// Message is Error::message: what went wrong.
func (e *DeError) Message() string {
	return e.inner.Message()
}

// Span is Error::span: the start/end index into the original document where
// the error occurred.
func (e *DeError) Span() rs.Option[tomledit.Range] {
	return e.inner.Span()
}

// DeErrorCustom is `impl serde::de::Error for Error`: `Error::custom(msg)`.
func DeErrorCustom(msg string) *DeError {
	return deErrorNew(tomledit.DeErrorCustom(msg))
}

// Error is `impl Display for Error`.
func (e *DeError) Error() string {
	return e.inner.Error()
}

// String is `impl Display for Error`.
func (e *DeError) String() string {
	return e.inner.Error()
}

// FmtDisplay is `impl Display for Error`, which delegates to the inner
// error's.
func (e *DeError) FmtDisplay(f *rs.Formatter) error {
	return e.inner.FmtDisplay(f)
}

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

// Deserializer is toml::Deserializer (toml::de::Deserializer):
// deserialization TOML document.
//
// To deserializes TOML values, instead of documents, see `ValueDeserializer`.
type Deserializer struct {
	deForward
	input string
}

// DeserializerNew is Deserializer::new: deserialization implementation for
// TOML.
func DeserializerNew(input string) *Deserializer {
	d := &Deserializer{deForward: deForward{deserializeAny: nil}, input: input}
	d.deserializeAny = d.DeserializeAny
	return d
}

// parse is `self.input.parse::<toml_edit::de::Deserializer>()
// .map_err(Error::new)`.
func (d *Deserializer) parse() (*tomledit.DeDeserializer, error) {
	inner, err := tomledit.DeDeserializerFromStr(d.input)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return inner, nil
}

// wrapEdit is `.map_err(Error::new)` on what the toml_edit deserializer
// returned.
func wrapEdit(value any, err error) (any, error) {
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *Deserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeAny(visitor))
}

// `None` is interpreted as a missing field so be sure to implement `Some`
// as a present field.
func (d *Deserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeOption(visitor))
}

func (d *Deserializer) DeserializeNewtypeStruct(name string, visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeNewtypeStruct(name, visitor))
}

func (d *Deserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeStruct(name, fields, visitor))
}

// Called when the type to deserialize is an enum, as opposed to a field in the type.
func (d *Deserializer) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeEnum(name, variants, visitor))
}

// DeValueDeserializer is toml::de::ValueDeserializer: deserialization TOML
// value.
//
//	#[derive(Deserialize)]
//	struct Config {
//	    title: String,
//	    owner: Owner,
//	}
//
//	#[derive(Deserialize)]
//	struct Owner {
//	    name: String,
//	}
//
//	let config = Config::deserialize(toml::de::ValueDeserializer::new(
//	    r#"{ title = 'TOML Example', owner = { name = 'Lisa' } }"#
//	)).unwrap();
//
//	assert_eq!(config.title, "TOML Example");
//	assert_eq!(config.owner.name, "Lisa");
type DeValueDeserializer struct {
	deForward
	input string
}

// DeValueDeserializerNew is ValueDeserializer::new: deserialization
// implementation for TOML.
func DeValueDeserializerNew(input string) *DeValueDeserializer {
	d := &DeValueDeserializer{deForward: deForward{deserializeAny: nil}, input: input}
	d.deserializeAny = d.DeserializeAny
	return d
}

// parse is `self.input.parse::<toml_edit::de::ValueDeserializer>()
// .map_err(Error::new)`.
func (d *DeValueDeserializer) parse() (*tomledit.DeValueDeserializer, error) {
	inner, err := tomledit.DeValueDeserializerFromStr(d.input)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return inner, nil
}

func (d *DeValueDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeAny(visitor))
}

// `None` is interpreted as a missing field so be sure to implement `Some`
// as a present field.
func (d *DeValueDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeOption(visitor))
}

func (d *DeValueDeserializer) DeserializeNewtypeStruct(name string, visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeNewtypeStruct(name, visitor))
}

func (d *DeValueDeserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeStruct(name, fields, visitor))
}

// Called when the type to deserialize is an enum, as opposed to a field in the type.
func (d *DeValueDeserializer) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	inner, err := d.parse()
	if err != nil {
		return nil, err
	}
	return wrapEdit(inner.DeserializeEnum(name, variants, visitor))
}

var (
	_ serde.Deserializer = (*Deserializer)(nil)
	_ serde.Deserializer = (*DeValueDeserializer)(nil)
)
