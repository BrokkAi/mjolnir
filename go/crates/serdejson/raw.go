// Port of serde_json 1.0.150 src/raw.rs

package serdejson

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// RawValue is `serde_json::value::RawValue`: a slice of syntactically
// valid JSON text, kept as it is. A RawValue serializes as its text,
// unchanged, and deserializing one keeps the text of the value without
// parsing it further. Rust's `Box<RawValue>` is a *RawValue.
type RawValue struct {
	json string
}

// rawValueToken is `TOKEN`, the name through which a RawValue reaches the
// Serializer and Deserializer that know it.
const rawValueToken = "$serde_json::private::RawValue"

// RawValueNull, RawValueTrue and RawValueFalse are `RawValue::NULL`,
// `RAW_VALUE::TRUE` and `RawValue::FALSE`.
var (
	RawValueNull  = &RawValue{json: "null"}
	RawValueTrue  = &RawValue{json: "true"}
	RawValueFalse = &RawValue{json: "false"}
)

// RawValueFromString is `RawValue::from_string`: convert an owned string of
// JSON data to a raw value. Any whitespace around the value is removed. It
// fails when the string is not one valid JSON value.
func RawValueFromString(json string) (*RawValue, error) {
	borrowed, err := FromStr[*RawValue](json)
	if err != nil {
		return nil, err
	}
	if len(borrowed.json) < len(json) {
		return &RawValue{json: borrowed.json}, nil
	}
	return &RawValue{json: json}, nil
}

// ToRawValue is `serde_json::value::to_raw_value`: convert a value into a
// RawValue holding its compact JSON text.
func ToRawValue(value any) (*RawValue, error) {
	text, err := ToString(value)
	if err != nil {
		return nil, err
	}
	return &RawValue{json: text}, nil
}

// Get is `RawValue::get`: access the JSON text underlying a raw value.
func (r *RawValue) Get() string { return r.json }

// String is `impl Display for RawValue`.
func (r *RawValue) String() string { return r.json }

// FmtDisplay is `impl Display for RawValue`.
func (r *RawValue) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(r.json) }

// FmtDebug is `impl Debug for RawValue`: RawValue(<text>).
func (r *RawValue) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr("RawValue(" + r.json + ")")
}

// SerializeSerde is `impl Serialize for RawValue`.
func (r *RawValue) SerializeSerde(serializer serde.Serializer) (any, error) {
	s, err := serializer.SerializeStruct(rawValueToken, 1)
	if err != nil {
		return nil, err
	}
	if err := s.SerializeField(rawValueToken, r.json); err != nil {
		return nil, err
	}
	return s.End()
}

// DeserializeSerde is `impl Deserialize for Box<RawValue>`.
func (r *RawValue) DeserializeSerde(deserializer serde.Deserializer) error {
	_, err := deserializer.DeserializeNewtypeStruct(rawValueToken, &boxedVisitor{out: r})
	return err
}

// boxedVisitor is the `BoxedVisitor` of `Box<RawValue>`.
type boxedVisitor struct{ out *RawValue }

func (v *boxedVisitor) Expecting() string { return "any valid JSON value" }

func (v *boxedVisitor) VisitMap(m serde.DeMapAccess) (any, error) {
	_, ok, err := m.NextKeySeed(rawKeySeed{})
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, serde.DeErrorInvalidType(&serde.DeUnexpectedMap{}, v)
	}
	_, err = m.NextValueSeed(&boxedFromString{out: v.out})
	return nil, err
}

// rawKeySeed is `RawKey`: the one key of the map a raw value comes in.
type rawKeySeed struct{}

func (rawKeySeed) DeserializeSeed(deserializer serde.Deserializer) (any, error) {
	return deserializer.DeserializeIdentifier(rawKeyVisitor{})
}

type rawKeyVisitor struct{}

func (rawKeyVisitor) Expecting() string { return "raw value" }

func (rawKeyVisitor) VisitStr(s string) (any, error) {
	if s == rawValueToken {
		return nil, nil
	}
	return nil, serde.DeErrorCustom("unexpected raw value")
}

// boxedFromString is `BoxedFromString`.
type boxedFromString struct{ out *RawValue }

func (s *boxedFromString) DeserializeSeed(deserializer serde.Deserializer) (any, error) {
	return deserializer.DeserializeStr(s)
}

func (s *boxedFromString) Expecting() string { return "raw value" }

func (s *boxedFromString) VisitStr(text string) (any, error) {
	s.out.json = text
	return nil, nil
}

// rawDeserializer is `BorrowedRawDeserializer` and `OwnedRawDeserializer`:
// the map with one entry, the token and the raw text, in which a
// Deserializer hands a raw value to its visitor.
type rawDeserializer struct {
	rawValue string
	present  bool
}

func (d *rawDeserializer) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if !d.present {
		return nil, false, nil
	}
	key, err := seed.DeserializeSeed(rawKeyDeserializer{})
	if err != nil {
		return nil, false, toError(err)
	}
	return key, true, nil
}

func (d *rawDeserializer) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	raw := d.rawValue
	d.present = false
	return leaveAsError(seed.DeserializeSeed(borrowedStrDeserializer(raw)))
}

func (d *rawDeserializer) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(d, keySeed, valueSeed)
}

func (d *rawDeserializer) SizeHint() rs.Option[int] { return rs.None[int]() }

// rawKeyDeserializer is `RawKeyDeserializer`: it gives every visitor the
// token as a borrowed str.
type rawKeyDeserializer struct{}

func (rawKeyDeserializer) any(v serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitBorrowedStr(v, rawValueToken))
}

func (d rawKeyDeserializer) DeserializeAny(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeBool(v serde.DeVisitor) (any, error)    { return d.any(v) }
func (d rawKeyDeserializer) DeserializeI8(v serde.DeVisitor) (any, error)      { return d.any(v) }
func (d rawKeyDeserializer) DeserializeI16(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeI32(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeI64(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeU8(v serde.DeVisitor) (any, error)      { return d.any(v) }
func (d rawKeyDeserializer) DeserializeU16(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeU32(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeU64(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeF32(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeF64(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeChar(v serde.DeVisitor) (any, error)    { return d.any(v) }
func (d rawKeyDeserializer) DeserializeStr(v serde.DeVisitor) (any, error)     { return d.any(v) }
func (d rawKeyDeserializer) DeserializeString(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d rawKeyDeserializer) DeserializeBytes(v serde.DeVisitor) (any, error)   { return d.any(v) }
func (d rawKeyDeserializer) DeserializeByteBuf(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d rawKeyDeserializer) DeserializeOption(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d rawKeyDeserializer) DeserializeUnit(v serde.DeVisitor) (any, error)    { return d.any(v) }
func (d rawKeyDeserializer) DeserializeUnitStruct(_ string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d rawKeyDeserializer) DeserializeNewtypeStruct(_ string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d rawKeyDeserializer) DeserializeSeq(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d rawKeyDeserializer) DeserializeTuple(_ int, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d rawKeyDeserializer) DeserializeTupleStruct(_ string, _ int, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d rawKeyDeserializer) DeserializeMap(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d rawKeyDeserializer) DeserializeStruct(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d rawKeyDeserializer) DeserializeEnum(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d rawKeyDeserializer) DeserializeIdentifier(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d rawKeyDeserializer) DeserializeIgnoredAny(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d rawKeyDeserializer) IsHumanReadable() bool                                { return true }

// borrowedStrDeserializer is serde's `BorrowedStrDeserializer` with this
// crate's error: a str that lives as long as the input.
type borrowedStrDeserializer string

func (d borrowedStrDeserializer) any(v serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitBorrowedStr(v, string(d)))
}

func (d borrowedStrDeserializer) DeserializeAny(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeBool(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeI8(v serde.DeVisitor) (any, error)   { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeI16(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeI32(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeI64(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeU8(v serde.DeVisitor) (any, error)   { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeU16(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeU32(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeU64(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeF32(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeF64(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeChar(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeStr(v serde.DeVisitor) (any, error)  { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeString(v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeBytes(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeByteBuf(v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeOption(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeUnit(v serde.DeVisitor) (any, error)   { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeUnitStruct(_ string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeNewtypeStruct(_ string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeSeq(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeTuple(_ int, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeTupleStruct(_ string, _ int, v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeMap(v serde.DeVisitor) (any, error) { return d.any(v) }
func (d borrowedStrDeserializer) DeserializeStruct(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return d.any(v)
}

// DeserializeEnum is `BorrowedStrDeserializer::deserialize_enum`: the str is
// a unit variant.
func (d borrowedStrDeserializer) DeserializeEnum(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return leaveAsError(serde.DeVisitorVisitEnum(v, &unitOnlyEnum{variant: string(d)}))
}
func (d borrowedStrDeserializer) DeserializeIdentifier(v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) DeserializeIgnoredAny(v serde.DeVisitor) (any, error) {
	return d.any(v)
}
func (d borrowedStrDeserializer) IsHumanReadable() bool { return true }

// unitOnlyEnum is the EnumAccess of a str deserializer: the str names a
// unit variant (`UnitOnly`).
type unitOnlyEnum struct {
	variant string
	owned   bool
}

func (e *unitOnlyEnum) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	var d serde.Deserializer = borrowedStrDeserializer(e.variant)
	if e.owned {
		d = stringDeserializer(e.variant)
	}
	value, err := seed.DeserializeSeed(d)
	if err != nil {
		return nil, nil, toError(err)
	}
	return value, unitOnly{}, nil
}

// unitOnly is `UnitOnly`.
type unitOnly struct{}

func (unitOnly) UnitVariant() error { return nil }

func (unitOnly) NewtypeVariantSeed(serde.DeDeserializeSeed) (any, error) {
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("newtype variant")))
}

func (unitOnly) TupleVariant(int, serde.DeVisitor) (any, error) {
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("tuple variant")))
}

func (unitOnly) StructVariant([]string, serde.DeVisitor) (any, error) {
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("struct variant")))
}
