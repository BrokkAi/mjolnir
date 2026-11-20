// Port of serde_core 1.0.229 src/de/value.rs and src/private/de.rs: the
// deserializers toml's value module gets from `serde::de::value` through
// IntoDeserializer (`String::into_deserializer()` and
// `Vec<Value>::into_deserializer()`) and the UnitOnly variant access a
// string hands to an enum's visitor.
//
// go/crates/serde does not provide `serde::de::value` yet, so these live here
// unexported (go/crates/tomledit keeps the same for toml_edit). Their error
// type is the format's (toml::de::Error): an error a visitor or seed returns
// is passed on, and taken as the format's by the format method that called
// them; an error they make themselves is the format's already.

package toml

import (
	"strconv"

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

// seqDeserializer is serde::de::value::SeqDeserializer over the values of
// an array: what `Vec<Value>::into_deserializer()` gives.
type seqDeserializer struct {
	deForward
	iter  []Value
	count int
}

func seqDeserializerNew(values []Value) *seqDeserializer {
	d := &seqDeserializer{deForward: deForward{deserializeAny: nil}, iter: values, count: 0}
	d.deserializeAny = d.DeserializeAny
	return d
}

// end is SeqDeserializer::end: check for remaining elements after passing
// the deserializer to `Visitor::visit_seq`.
func (d *seqDeserializer) end() error {
	remaining := len(d.iter)
	if remaining == 0 {
		return nil
	}
	// First argument is the number of elements in the data, second
	// argument is the number of elements expected by the Deserialize.
	return deErrorFrom(serde.DeErrorInvalidLength(d.count+remaining, expectedInSeq(d.count)))
}

func (d *seqDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	v, err := serde.DeVisitorVisitSeq(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	if err := d.end(); err != nil {
		return nil, err
	}
	return v, nil
}

func (d *seqDeserializer) NextElementSeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if len(d.iter) == 0 {
		return nil, false, nil
	}
	value := d.iter[0]
	d.iter = d.iter[1:]
	d.count++
	element, err := seed.DeserializeSeed(valueIntoDeserializer(value))
	if err != nil {
		return nil, false, deErrorFrom(err)
	}
	return element, true, nil
}

// SizeHint is `size_hint::from_bounds` of a vector's iterator: exact.
func (d *seqDeserializer) SizeHint() rs.Option[int] {
	return rs.Some(len(d.iter))
}

// expectedInSeq is ExpectedInSeq.
type expectedInSeq int

func (e expectedInSeq) Expecting() string {
	if e == 1 {
		return "1 element in sequence"
	}
	return strconv.Itoa(int(e)) + " elements in sequence"
}

// unitOnly is serde's private::UnitOnly: the VariantAccess of an enum read
// from a bare string, which can only be a unit variant.
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
