// Port of toml 0.8.2 src/lib.rs

package toml

// A [serde]-compatible [TOML]-parsing library
//
// TOML itself is a simple, ergonomic, and readable configuration format:
//
//	[package]
//	name = "toml"
//	version = "0.4.2"
//	authors = ["Alex Crichton <alex@alexcrichton.com>"]
//
//	[dependencies]
//	serde = "1.0"
//
// The TOML format tends to be relatively common throughout the Rust community
// for configuration, notably being used by [Cargo], Rust's package manager.
//
// ## TOML values
//
// A TOML document is represented with the [`Table`] type which maps `String` to the [`Value`] enum:
//
//	pub enum Value {
//	    String(String),
//	    Integer(i64),
//	    Float(f64),
//	    Boolean(bool),
//	    Datetime(Datetime),
//	    Array(Array),
//	    Table(Table),
//	}
//
// ## Parsing TOML
//
// The easiest way to parse a TOML document is via the [`Table`] type:
//
//	use toml::Table;
//
//	let value = "foo = 'bar'".parse::<Table>().unwrap();
//
//	assert_eq!(value["foo"].as_str(), Some("bar"));
//
// The [`Table`] type implements a number of convenience methods and
// traits; the example above uses [`FromStr`] to parse a [`str`] into a
// [`Table`].
//
// ## Deserialization and Serialization
//
// This crate supports [`serde`] 1.0 with a number of
// implementations of the `Deserialize`, `Serialize`, `Deserializer`, and
// `Serializer` traits. Namely, you'll find:
//
//   - `Deserialize for Table`
//   - `Serialize for Table`
//   - `Deserialize for Value`
//   - `Serialize for Value`
//   - `Deserialize for Datetime`
//   - `Serialize for Datetime`
//   - `Deserializer for de::Deserializer`
//   - `Serializer for ser::Serializer`
//   - `Deserializer for Table`
//   - `Deserializer for Value`
//
// This means that you can use Serde to deserialize/serialize the
// [`Table`] type as well as [`Value`] and [`Datetime`] type in this crate. You can also
// use the [`Deserializer`], [`Serializer`], or [`Table`] type itself to act as
// a deserializer/serializer for arbitrary types.
//
// The crate root re-exports `de::{from_str, Deserializer}` (FromStr,
// Deserializer), `ser::{to_string, to_string_pretty, Serializer}` (ToString,
// ToStringPretty, Serializer), `value::Value` (Value), `table::Table`
// (Table) and `serde_spanned::Spanned`, which this port does not provide
// (see doc.go). In Go they are the same names.
