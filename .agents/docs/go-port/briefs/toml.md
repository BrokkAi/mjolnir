# `go/crates/toml` (toml 0.8.2) and `go/crates/tomledit` (toml_edit 0.20.2)

Oracle module `go/port/oracle/src/toml_oracle.rs`, feature `toml`.

Mjolnir's configuration is TOML read through serde-derived types
(`mj-core/src/config*.rs`), and its errors for a bad config file are shown to
users and asserted in tests (`mj-core/src/config/tests.rs`). A few places edit
a config file in place and must keep its comments and layout (`toml_edit`:
grep `mj-core` and `mj-worker-staging`). Check in `Cargo.lock` which
`toml_edit` version `toml` 0.8.2 parses with; if it differs from the 0.20.2
used directly, both behaviours may need reproducing.

1. The parser, with the crate's acceptance and rejection and its error
   messages with the rendered source excerpt:

       TOML parse error at line 3, column 9
         |
       3 | port = "x"
         |        ^^^
       invalid type: string "x", expected u16

   Syntax errors and data errors raised during deserialization (with the span
   of the offending key or value) byte for byte, including span arithmetic for
   multi-line and non-ASCII content.
2. `toml::Value`, `toml::Table` (find whether `preserve_order` is on and
   reproduce the map's order), `Datetime`, the accessors and mutators used,
   `Display` of a value, `from_str::<Value>`.
3. `to_string` and `to_string_pretty` byte for byte (table ordering, inline vs
   standard tables, arrays, quoting and escaping, floats and datetimes), and
   the serializer's errors.
4. The serde bridge through `go/crates/serde` (read its `doc.go` for the
   interface a format implements): `toml.FromStr[T]`, `toml.ToString`,
   `toml.ToStringPretty`, and the `Deserializer` used with `serde_ignored`
   (see `mj-core/src/config/loading.rs`). If the interface you need is
   missing, write the bridge against what you need, say precisely what in the
   report, and leave it behind `rs.Todo`.
5. `tomledit`: `Document` (parse; `to_string` preserving every byte of
   formatting and comments), `Item`, `Value`, `Table`, `InlineTable`, `Array`,
   `value(..)`, indexing, insertion and removal with the crate's decor rules
   (whitespace and comments given to a new key or table, and where it goes), as
   far as Mjolnir uses them.

Tests: the crates' suites (`toml`'s `tests/` with the `toml-test` corpus it
ships, and `toml_edit`'s), and differential tests: generated valid and invalid
documents (truncation, bad escapes, duplicate keys, mixed types, bad
datetimes, deep nesting, non-ASCII) comparing parse results and full error
text; serialization in both styles; serde-derived types defined identically on
both sides (options, enums in each tagging style, nested tables, arrays of
tables, `deny_unknown_fields`, defaults, flatten), data errors with spans; edit
scripts on documents with comments, comparing the text.
