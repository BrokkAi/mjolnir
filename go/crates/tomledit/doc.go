// Package tomledit stands in for the Rust crate toml_edit 0.20.2 with its
// `serde` feature (as toml 0.8.2 enables it), and for toml_datetime 0.6.3,
// which toml_edit re-exports whole (`pub use toml_datetime::*`). It is the
// toml_edit both Mjolnir (mj-core/src/config/document.rs,
// mj-worker-staging/src/worker_binary/catalog.rs) and go/crates/toml use.
//
// Covered, by Rust path:
//
//   - the parser (src/parser/*.rs over the parts of winnow 0.5.40 it uses,
//     winnow.go): Document, Key, Value and Item from text, with the crate's
//     acceptance and rejection, its recursion limit, and TomlError with its
//     message, span and rendered source excerpt;
//   - Document (DocumentFromStr, Display preserving every byte of layout and
//     comments, Index, Deref to its root table, trailing), Item and the
//     `value`, `table` and `array` constructors (ItemValue, ItemTable,
//     ItemArray), Value, Table, InlineTable, Array, ArrayOfTables, Key,
//     KeyMut, TableLike, the Entry APIs, RawString, Decor, Repr, Formatted,
//     the encoder (src/encode.rs) and its default decor for new keys,
//     values and tables, Table::position and the order tables are written
//     in, as far as Mjolnir and toml use them and their common methods;
//   - toml_datetime: Datetime, Date, Time, Offset, DatetimeParseError,
//     Display, FromStr, and the Serialize and Deserialize impls with their
//     private names (DatetimeName, DatetimeField, DatetimeFromString);
//   - toml_edit::de: DeFromStr, DeFromSlice, DeFromDocument, DeDeserializer,
//     DeValueDeserializer and the private table, array, key, datetime,
//     spanned and enum deserializers, with the crate's spans and the keys
//     its errors carry; DeError;
//   - toml_edit::ser: SerToString, SerToStringPretty, SerToVec,
//     SerToDocument, SerValueSerializer and the private map, key, array and
//     variant serializers; SerError;
//   - toml_edit::visit_mut: VisitMut and the VisitMutVisit* functions that
//     are its default methods (the pretty serializer and toml's formatter
//     are visitors);
//   - serde_spanned's private names (SpannedName, SpannedIsSpanned), which
//     the deserializers recognise.
//
// Not covered: toml_edit::visit (the read-only visitor; nothing uses it),
// the `perf` and `unbounded` features, and serde_spanned::Spanned itself.
//
// serde's `de::value` deserializers that toml_edit takes through
// IntoDeserializer (StrDeserializer, StringDeserializer,
// BorrowedStrDeserializer, UsizeDeserializer) are not in go/crates/serde
// yet; serde_de_value.go keeps unexported copies.
package tomledit
