# `go/crates/serde` (serde 1.0.229) and `go/crates/serdejson` (serde_json 1.0.150)

Oracle modules `go/port/oracle/src/serde_derive.rs` (feature `serde_derive`)
and `serde_json_oracle.rs` (feature `serde_json`).

Mjolnir's wire messages, journal entries, database columns and config files
are serde-derived types (about 1,000, with 2,100 attributes). A Rust daemon
drives a Go worker, some code measures the byte length of its own encoded
messages (`mj-core/src/relay/protocol.rs`, `transfer_chunk_capacity`), digests
cover encoded bytes, and serde's error text reaches users and tests. Encoding
must be byte-identical, and decoding must accept, reject and report exactly as
Rust does.

The shape of ported types is fixed (rule 9 of the translation rules and
`go/rs/registry.go`): plain structs; sealed interfaces with one struct per
variant (values are pointers to variant structs); defined integer types for
data-free enums; defined types for newtypes. Field names and attributes come
from tags: `rs:"rust_name"` and `serde:"..."`, which holds the field's serde
attributes in serde's own spelling, comma-separated (`default`,
`default=path::to::fn`, `skip_serializing_if=Option::is_none`, `rename=x`,
`alias=y`, `flatten`, `skip`, `skip_serializing`, `skip_deserializing`,
`with=module::path`, `serialize_with=..`, `deserialize_with=..`). Container
and variant attributes and the variant list come from the registry
(`rs.TypeInfoOf`, `TypeInfo.Attrs`, `.Variants`, `.Shape`). You own and
document the exact tag grammar. The declarations generator
(`go/port/declgen.py`) already writes these tags and registrations: read a few
generated files to see them.

Rules that hold throughout:
- A Rust name comes only from the registry or the `rs` tag, never from a Go
  identifier (product types are named `<ModulePrefix>_<RustName>` in Go),
  except as the documented fallback for an unregistered type.
- A nil Go slice is an empty `Vec`: it serializes as `[]`, never `null`; a
  `Vec<u8>` is an array of numbers as serde_json writes it. A zero-value
  collection from `go/rs/collections` is an empty map or set. Only
  `rs.Option` is ever `null`. A nil pointer or interface in a field whose Rust
  type is not an `Option` is a porting error: panic naming the type and field.
- Functions named by attributes are registered by Rust path
  (`serde.RegisterFn("crate::module::name", fn)` with typed helpers), with
  built-ins for the std paths that appear. Grep every distinct serde attribute
  and every `with`, `default` and `skip_serializing_if` target in `mj-*/` and
  list them in `doc.go`.
- Hand-written `Serialize`/`Deserialize` impls implement Go interfaces you
  define (`SerializeSerde`, `DeserializeSerde`) in terms of the data model.
- Foreign Go types implementing `json.Marshaler`/`json.Unmarshaler` (the ACP
  types of `github.com/BrokkAi/acp-go`, already byte-exact with Rust) pass
  through the JSON format unchanged.
- `rs.Option`, `rs.Tuple2..5`, `rs.Unit`, the collections (through the
  reflect interfaces), `[]byte`, `time.Duration` and `time.Time` (serde's
  `Duration` and `SystemTime` forms), strings for paths. Unexported fields are
  serialized like any other.
- `serde_json::Value` is the `jsonvalue` representation of
  `github.com/BrokkAi/acp-go/jsonvalue`, exposed as `serdejson.Value` with
  `serdejson.Map` as `jsonvalue.Object`, and the `Value` API the Rust code
  uses. Mjolnir builds serde_json with `preserve_order`, `float_roundtrip` and
  `raw_value`; acp-go's decoder is built without `float_roundtrip`: find out
  exactly what it changes and match serde_json with it.

`serde`, the engine: struct, newtype, tuple and unit structs; enums externally,
internally (`tag`), adjacently (`tag` + `content`) tagged and untagged;
`rename_all` (every convention) and `rename_all_fields`;
`deny_unknown_fields`; `default` on containers and fields; `flatten` (into
maps, and its interplay with `deny_unknown_fields`); `transparent`; `other`;
`alias`; `skip*`; `try_from`/`from`/`into`; `remote` if used. Errors with
serde's exact text (missing, duplicate and unknown fields and variants,
invalid type, invalid value, invalid length, `data did not match any variant
of untagged enum X`), integer range and float/integer coercion rules,
`Option` for missing fields, and `serde_ignored` 0.1.14
(`serde.DeserializeIgnored`, reporting each ignored key's path as that crate
formats it). The engine is shared with TOML and YAML: expose a clear
serializer/deserializer interface a format implements, described at the top
of `doc.go` (the TOML port plugs into it).

`serdejson`: `ToString`, `ToStringPretty`, `ToVec`, `ToVecPretty`,
`ToWriter`, `ToValue`, `FromStr[T]`, `FromSlice[T]`, `FromReader[T]`,
`FromValue[T]`, a `Json(...)` builder for `json!` that keeps insertion order,
`RawValue`, `Number`, the streaming deserializer if used, and `Error` with
`Line`, `Column`, `Classify`, `IsEof` and serde_json's exact `Display` (`...
at line 1 column 15`). Output bytes equal serde_json's (compact and pretty,
ryu float formatting, NaN and infinity as `null`, escaping, map key rules).
Parsing accepts and rejects exactly what serde_json does (recursion limit 128,
trailing characters, duplicate keys per target type, lone surrogates, number
ranges and `float_roundtrip` precision) and reports the same line and column
for every error, including data errors raised by derived code (internally
tagged, untagged and flattened paths buffer and so report differently).

Tests: serde's `test_suite` and serde_json's `tests/` where they apply, and
large differential tests: types covering every attribute combination defined
identically in the oracle module and in Go; generated values round-tripped
through both requiring identical bytes (compact and pretty); generated and
mutated JSON (truncated, wrong types, unknown, missing and duplicate fields,
out-of-range numbers, bad escapes, deep nesting) requiring identical success
or identical error text and position; `Value` parse and re-encode of random
documents and number corpora.

If the whole scope cannot be finished in one go, finish the engine and JSON
for derived types first, with exact errors, and report precisely what remains.
