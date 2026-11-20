// Package toml stands in for the Rust crate toml 0.8.2, built as Mjolnir
// builds it: default features (`parse`, `display`), without
// `preserve_order`. It parses and writes through go/crates/tomledit
// (toml_edit 0.20.2, the version toml 0.8.2 depends on in Mjolnir's
// Cargo.lock) and reads and writes Rust types through go/crates/serde.
//
// Covered, by Rust path:
//
//   - toml::from_str (FromStr[T]), toml::Deserializer (DeserializerNew; it
//     is the serde.Deserializer serde_ignored wraps), toml::de::Error
//     (DeError: Display with the source excerpt, message, span),
//     toml::de::ValueDeserializer.
//   - toml::to_string, toml::to_string_pretty (ToString, ToStringPretty),
//     toml::Serializer (SerializerNew, SerializerPretty),
//     toml::ser::ValueSerializer, toml::ser::Error (SerError).
//   - toml::Value: the enum (*Value_String, ...), get/get_mut and indexing
//     by key or position, the as_*/is_* accessors, same_type, type_str,
//     try_from (ValueTryFrom), try_into (ValueTryInto[T]), the From impls
//     (ValueFrom*, IntoValue), Display, FromStr, Serialize, Deserialize,
//     and the Value as a Deserializer (ValueIntoDeserializer).
//   - toml::Table (toml::map::Map<String, Value>): a BTreeMap, so keys
//     iterate in byte order; new, get, get_mut, insert, remove, retain,
//     entry (MapEntry and its variants), len, iter, iter_mut, keys,
//     values, into_iter, FromIterator (TableFromIter), Extend, Index,
//     IndexMut, Clone, PartialEq, Debug, Display, FromStr, try_from,
//     try_into, Serialize, Deserialize, and the Table as a Deserializer.
//   - toml::value::{Array, Table, Datetime, Date, Time, Offset,
//     DatetimeParseError} as aliases of the tomledit types.
//
// Not covered:
//
//   - the `toml!` macro and the hidden helpers in src/macros.rs: Mjolnir
//     does not use them;
//   - serde_spanned::Spanned (`toml::Spanned`): Mjolnir does not use it;
//     the deserializers recognise its serde names (tomledit.SpannedName and
//     friends) as toml_edit does;
//   - the `preserve_order` feature.
//
// serde's `de::value` deserializers that toml takes through
// IntoDeserializer (`String::into_deserializer()`, a Vec of Values) are not
// in go/crates/serde yet; serde_de_value.go keeps unexported copies.
package toml
