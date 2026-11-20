# YAML and JSON-with-comments parsers

Packages: `go/crates/yamlrust2` (yaml-rust2 0.13.0, `default-features =
false`) and `go/crates/jsoncparser` (jsonc-parser 0.34.0, features `cst`,
`serde`). Oracle modules `yaml.rs` (feature `yaml`) and `jsonc.rs` (`jsonc`).

Mjolnir reads other tools' configuration files with these crates: YAML and
JSONC files written by the coding agents it manages (`grep -rn
'yaml_rust2\|YamlLoader\|jsonc_parser' --include='*.rs' mj-*`). The Go port
must read the same files to the same values, reject what Rust rejects with the
same messages, and, where the code edits a JSONC file through the CST, write
the same bytes.

`yamlrust2`: port the scanner, parser and loader (`scanner.rs`, `parser.rs`,
`yaml.rs`; `emitter.rs` only if Mjolnir emits YAML). `YamlLoader::load_from_str`
(multi-document), the `Yaml` enum (`Real` kept as its source string, `Integer`,
`String`, `Boolean`, `Array`, `Hash` as an insertion-ordered map with the
crate's duplicate-key behaviour, `Alias`, `Null`, `BadValue`), the accessors
used, indexing that yields `BadValue`, this version's scalar typing rules,
anchors and aliases, block and flow styles, chomping and indentation
indicators, escapes, and `ScanError` with its exact `Display`. YAML libraries
disagree in corner cases, so this is a port of this crate, not an adapter.

`jsoncparser`: `parse_to_serde_value(text, &ParseOptions)` with every option
and default, returning the port's JSON value (the representation of
`github.com/BrokkAi/acp-go/jsonvalue`, exposed as `serdejson.Value`), the
crate's number conversion, duplicate keys, and `ParseError` with its exact
`Display`. The `cst` module as far as Mjolnir uses it: `CstRootNode::parse`,
navigation, `set_value`, `append`, `remove`, `ensure_multiline` and friends,
and `to_string()` byte-identical, including whitespace, indentation detection,
newline style and trailing-comma decisions on insertion.

Tests: both crates' own tests (yaml-rust2 ships the YAML test-suite driver
under `tests/`). Differential tests on generated and mutated documents (every
scalar style, nesting, anchors, comments in every position, invalid
constructs, Unicode, CRLF, tabs, deep nesting), comparing a canonical JSON
rendering of the value and the full error text; for the CST, generated edit
scripts on generated JSONC documents, comparing the text byte for byte.
