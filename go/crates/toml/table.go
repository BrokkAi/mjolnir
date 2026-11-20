// Port of toml 0.8.2 src/table.rs

package toml

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Type representing a TOML table, payload of the `Value::Table` variant.
// By default it is backed by a BTreeMap, enable the `preserve_order` feature
// to use a LinkedHashMap instead.
//
// (`pub type Table = Map<String, Value>`: the type is in map.go.)

// TableTryFrom is Table::try_from: convert a `T` into `toml::Table`.
//
// This conversion can fail if `T`'s implementation of `Serialize` decides to
// fail, or if `T` contains a map with non-string keys.
func TableTryFrom(value any) (Table, error) {
	serialized, err := serde.Serialize(value, tableSerializer{})
	if err != nil {
		return Table{}, serErrorFrom(err)
	}
	return serialized.(Table), nil
}

// TableTryInto is Table::try_into: interpret a `toml::Table` as an instance
// of type `T`.
//
// This conversion can fail if the structure of the `Table` does not match the
// structure expected by `T`, for example if `T` is a bool which can't be
// mapped to a `Table`. It can also fail if the structure is correct but `T`'s
// implementation of `Deserialize` decides that something is wrong with the
// data, for example required struct fields are missing from the TOML map or
// some number is too big to fit in the expected primitive type.
func TableTryInto[T any](t Table) (T, error) {
	return deserializeWith[T](TableIntoDeserializer(t))
}

// String is `impl Display for Table`: `to_string` of the table, panicking
// with "Unable to represent value as string" when it cannot be serialized.
func (t *Table) String() string {
	text, err := ToString(t)
	return rs.Expect(rs.ResultOf(text, err), "Unable to represent value as string")
}

// FmtDisplay is `impl Display for Table`: the text padded to the formatter's
// flags (`.fmt(f)` on the String).
func (t *Table) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(t.String())
}

// TableFromStr is `impl FromStr for Table`: `crate::from_str(s)`.
func TableFromStr(s string) (Table, error) {
	return FromStr[Table](s)
}

// TableIntoDeserializer is `impl Deserializer for Table` and `impl
// IntoDeserializer for Table`: the table read back through serde, as
// `Value::Table(self)` is.
func TableIntoDeserializer(t Table) serde.Deserializer {
	return valueIntoDeserializer(&Value_Table{F0: t})
}
