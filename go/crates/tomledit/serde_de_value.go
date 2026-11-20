// Port of serde_core 1.0.229 src/de/value.rs and src/private/de.rs: the
// deserializers toml_edit's de module gets from `serde::de::value` through
// IntoDeserializer (`(&str).into_deserializer()`,
// `String::into_deserializer()`, `usize::into_deserializer()`,
// `BorrowedStrDeserializer::new`) and the UnitOnly variant access they hand
// to an enum's visitor.
//
// go/crates/serde does not provide `serde::de::value` yet, so these live here
// unexported. Their error type is the format's (toml_edit::de::Error): an
// error a visitor or seed returns is passed on, and taken as the format's by
// the format method that called them; an error they make themselves is the
// format's already.

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// deForward is serde's `forward_to_deserialize_any!`: the Deserializer
// methods that do nothing but call deserialize_any. A deserializer embeds it,
// set to its own DeserializeAny, and declares the methods it writes itself;
// those take precedence over the embedded ones.
type deForward struct {
	deserializeAny func(visitor serde.DeVisitor) (any, error)
}

func (d deForward) DeserializeBool(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeI8(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeI16(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeI32(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeI64(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeU8(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeU16(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeU32(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeU64(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeF32(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeF64(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeChar(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeStr(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeString(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeBytes(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeByteBuf(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeUnit(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeUnitStruct(_ string, visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeNewtypeStruct(_ string, visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeSeq(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeTuple(_ int, visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeTupleStruct(_ string, _ int, visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeMap(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeStruct(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeIdentifier(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

func (d deForward) DeserializeIgnoredAny(visitor serde.DeVisitor) (any, error) {
	return d.deserializeAny(visitor)
}

// IsHumanReadable is the default `Deserializer::is_human_readable`.
func (d deForward) IsHumanReadable() bool { return true }

// strDeserializer is serde::de::value::StrDeserializer: a deserializer
// holding a `&str`.
type strDeserializer struct {
	deForward
	value string
}

func strDeserializerNew(value string) *strDeserializer {
	d := &strDeserializer{deForward: deForward{deserializeAny: nil}, value: value}
	d.deserializeAny = d.DeserializeAny
	return d
}

func (d *strDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitStr(visitor, d.value)
}

func (d *strDeserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitEnum(visitor, d)
}

// VariantSeed is `impl EnumAccess for StrDeserializer`.
func (d *strDeserializer) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	value, err := seed.DeserializeSeed(d)
	if err != nil {
		return nil, nil, err
	}
	return value, unitOnly{}, nil
}

// borrowedStrDeserializer is serde::de::value::BorrowedStrDeserializer: a
// deserializer holding a `&str` with a lifetime tied to another
// deserializer.
type borrowedStrDeserializer struct {
	deForward
	value string
}

func borrowedStrDeserializerNew(value string) *borrowedStrDeserializer {
	d := &borrowedStrDeserializer{deForward: deForward{deserializeAny: nil}, value: value}
	d.deserializeAny = d.DeserializeAny
	return d
}

func (d *borrowedStrDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitBorrowedStr(visitor, d.value)
}

func (d *borrowedStrDeserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitEnum(visitor, d)
}

// VariantSeed is `impl EnumAccess for BorrowedStrDeserializer`.
func (d *borrowedStrDeserializer) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	value, err := seed.DeserializeSeed(d)
	if err != nil {
		return nil, nil, err
	}
	return value, unitOnly{}, nil
}

// stringDeserializer is serde::de::value::StringDeserializer: a deserializer
// holding a `String`.
type stringDeserializer struct {
	deForward
	value string
}

func stringDeserializerNew(value string) *stringDeserializer {
	d := &stringDeserializer{deForward: deForward{deserializeAny: nil}, value: value}
	d.deserializeAny = d.DeserializeAny
	return d
}

func (d *stringDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitString(visitor, d.value)
}

func (d *stringDeserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitEnum(visitor, d)
}

// VariantSeed is `impl EnumAccess for StringDeserializer`.
func (d *stringDeserializer) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	value, err := seed.DeserializeSeed(d)
	if err != nil {
		return nil, nil, err
	}
	return value, unitOnly{}, nil
}

// usizeDeserializer is serde::de::value::UsizeDeserializer: a deserializer
// holding a `usize`. Every method forwards to deserialize_any, which visits
// the value as a u64.
type usizeDeserializer struct {
	deForward
	value int
}

func usizeDeserializerNew(value int) *usizeDeserializer {
	d := &usizeDeserializer{deForward: deForward{deserializeAny: nil}, value: value}
	d.deserializeAny = d.DeserializeAny
	return d
}

func (d *usizeDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	return serde.DeVisitorVisitU64(visitor, uint64(d.value))
}

// unitOnly is serde's private::UnitOnly (and toml_edit's de::key::UnitOnly,
// which is the same): the VariantAccess of an enum read from a bare string,
// which can only be a unit variant.
type unitOnly struct{}

func (unitOnly) UnitVariant() error { return nil }

func (unitOnly) NewtypeVariantSeed(serde.DeDeserializeSeed) (any, error) {
	return nil, deErrorFrom(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("newtype variant")))
}

func (unitOnly) TupleVariant(int, serde.DeVisitor) (any, error) {
	return nil, deErrorFrom(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("tuple variant")))
}

func (unitOnly) StructVariant([]string, serde.DeVisitor) (any, error) {
	return nil, deErrorFrom(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("struct variant")))
}

// noSizeHint is the default `SeqAccess::size_hint` and
// `MapAccess::size_hint`: None.
func noSizeHint() rs.Option[int] { return rs.None[int]() }
