// Port of serde_core 1.0.229 src/de/mod.rs

package serde

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Deserializable is the Rust trait `Deserialize` for a type whose impl is
// written by hand: the method holds the body of `fn deserialize`, and stores
// the value in its receiver, which is a pointer to the zero value. A type
// whose Deserialize is derived needs no method; Deserialize reads its
// registration.
//
// An enum that is a Go interface cannot carry methods; its hand-written impl
// is registered with RegisterDeserialize instead.
type Deserializable interface {
	// DeserializeSerde is `fn deserialize<D: Deserializer<'de>>(deserializer:
	// D) -> Result<Self, D::Error>`.
	DeserializeSerde(deserializer Deserializer) error
}

// Deserializer is the Rust trait `serde::Deserializer`: a data format that
// can deserialize any data structure supported by serde.
//
// Each method is a hint about what the Deserialize is looking for; the
// format answers by calling one method of the visitor (through the
// DeVisitorVisit* functions, which supply the visitor's default methods) and
// returns what the visitor returned.
//
// An error a visitor or a seed returns may be a *Error, which no format has
// taken yet (see Error). A format turns it into its own error before it
// returns it.
type Deserializer interface {
	// DeserializeAny asks the format to look at the input and call the
	// visit method for whatever it finds.
	DeserializeAny(visitor DeVisitor) (any, error)
	// DeserializeBool hints that the Deserialize expects a `bool`.
	DeserializeBool(visitor DeVisitor) (any, error)
	// DeserializeI8 hints that the Deserialize expects an `i8`.
	DeserializeI8(visitor DeVisitor) (any, error)
	// DeserializeI16 hints that the Deserialize expects an `i16`.
	DeserializeI16(visitor DeVisitor) (any, error)
	// DeserializeI32 hints that the Deserialize expects an `i32`.
	DeserializeI32(visitor DeVisitor) (any, error)
	// DeserializeI64 hints that the Deserialize expects an `i64`.
	DeserializeI64(visitor DeVisitor) (any, error)
	// DeserializeU8 hints that the Deserialize expects a `u8`.
	DeserializeU8(visitor DeVisitor) (any, error)
	// DeserializeU16 hints that the Deserialize expects a `u16`.
	DeserializeU16(visitor DeVisitor) (any, error)
	// DeserializeU32 hints that the Deserialize expects a `u32`.
	DeserializeU32(visitor DeVisitor) (any, error)
	// DeserializeU64 hints that the Deserialize expects a `u64`.
	DeserializeU64(visitor DeVisitor) (any, error)
	// DeserializeF32 hints that the Deserialize expects an `f32`.
	DeserializeF32(visitor DeVisitor) (any, error)
	// DeserializeF64 hints that the Deserialize expects an `f64`.
	DeserializeF64(visitor DeVisitor) (any, error)
	// DeserializeChar hints that the Deserialize expects a `char`.
	DeserializeChar(visitor DeVisitor) (any, error)
	// DeserializeStr hints that the Deserialize expects a string and does
	// not benefit from owning it.
	DeserializeStr(visitor DeVisitor) (any, error)
	// DeserializeString hints that the Deserialize expects a string and
	// would benefit from owning it.
	DeserializeString(visitor DeVisitor) (any, error)
	// DeserializeBytes hints that the Deserialize expects a byte array and
	// does not benefit from owning it.
	DeserializeBytes(visitor DeVisitor) (any, error)
	// DeserializeByteBuf hints that the Deserialize expects a byte array and
	// would benefit from owning it.
	DeserializeByteBuf(visitor DeVisitor) (any, error)
	// DeserializeOption hints that the Deserialize expects an optional
	// value.
	DeserializeOption(visitor DeVisitor) (any, error)
	// DeserializeUnit hints that the Deserialize expects a unit value.
	DeserializeUnit(visitor DeVisitor) (any, error)
	// DeserializeUnitStruct hints that the Deserialize expects a unit struct
	// with a particular name.
	DeserializeUnitStruct(name string, visitor DeVisitor) (any, error)
	// DeserializeNewtypeStruct hints that the Deserialize expects a newtype
	// struct with a particular name.
	DeserializeNewtypeStruct(name string, visitor DeVisitor) (any, error)
	// DeserializeSeq hints that the Deserialize expects a sequence of
	// values.
	DeserializeSeq(visitor DeVisitor) (any, error)
	// DeserializeTuple hints that the Deserialize expects a sequence of
	// values and knows how many there are without looking at the input.
	DeserializeTuple(len int, visitor DeVisitor) (any, error)
	// DeserializeTupleStruct hints that the Deserialize expects a tuple
	// struct with a particular name and number of fields.
	DeserializeTupleStruct(name string, len int, visitor DeVisitor) (any, error)
	// DeserializeMap hints that the Deserialize expects a map of key-value
	// pairs.
	DeserializeMap(visitor DeVisitor) (any, error)
	// DeserializeStruct hints that the Deserialize expects a struct with a
	// particular name and fields.
	DeserializeStruct(name string, fields []string, visitor DeVisitor) (any, error)
	// DeserializeEnum hints that the Deserialize expects an enum value with
	// a particular name and possible variants.
	DeserializeEnum(name string, variants []string, visitor DeVisitor) (any, error)
	// DeserializeIdentifier hints that the Deserialize expects the name of a
	// struct field or the discriminant of an enum variant.
	DeserializeIdentifier(visitor DeVisitor) (any, error)
	// DeserializeIgnoredAny hints that the Deserialize needs a value of any
	// type only to skip it.
	DeserializeIgnoredAny(visitor DeVisitor) (any, error)
	// IsHumanReadable reports whether Deserialize impls should expect their
	// human-readable form. Rust's default is true.
	IsHumanReadable() bool
}

// DeDeserializer is `serde::de::Deserializer`, the path some Rust code writes
// for Deserializer.
type DeDeserializer = Deserializer

// DeVisitor is the Rust trait `de::Visitor`: what walks through the values
// of a Deserializer.
//
// Expecting is the trait's one required method: the noun phrase error
// messages put after "expected". Rust's `expecting` writes it to a formatter;
// every implementation writes one string, so the Go method returns it.
//
// The trait's other methods all have default bodies. A Go visitor implements
// only the ones it overrides, with exactly the signatures of the DeVisit*
// interfaces below, and formats call a visitor through the DeVisitorVisit*
// functions, which run the method when the visitor has it and the Rust
// default otherwise. A method returns the visitor's `Value` as `any`.
type DeVisitor interface {
	DeExpected
}

// The methods of `de::Visitor` a visitor may implement.
type (
	// DeVisitBool is `Visitor::visit_bool`.
	DeVisitBool interface {
		VisitBool(v bool) (any, error)
	}
	// DeVisitI8 is `Visitor::visit_i8`. The default forwards to visit_i64.
	DeVisitI8 interface {
		VisitI8(v int8) (any, error)
	}
	// DeVisitI16 is `Visitor::visit_i16`. The default forwards to visit_i64.
	DeVisitI16 interface {
		VisitI16(v int16) (any, error)
	}
	// DeVisitI32 is `Visitor::visit_i32`. The default forwards to visit_i64.
	DeVisitI32 interface {
		VisitI32(v int32) (any, error)
	}
	// DeVisitI64 is `Visitor::visit_i64`.
	DeVisitI64 interface {
		VisitI64(v int64) (any, error)
	}
	// DeVisitU8 is `Visitor::visit_u8`. The default forwards to visit_u64.
	DeVisitU8 interface {
		VisitU8(v uint8) (any, error)
	}
	// DeVisitU16 is `Visitor::visit_u16`. The default forwards to visit_u64.
	DeVisitU16 interface {
		VisitU16(v uint16) (any, error)
	}
	// DeVisitU32 is `Visitor::visit_u32`. The default forwards to visit_u64.
	DeVisitU32 interface {
		VisitU32(v uint32) (any, error)
	}
	// DeVisitU64 is `Visitor::visit_u64`.
	DeVisitU64 interface {
		VisitU64(v uint64) (any, error)
	}
	// DeVisitF32 is `Visitor::visit_f32`. The default forwards to visit_f64.
	DeVisitF32 interface {
		VisitF32(v float32) (any, error)
	}
	// DeVisitF64 is `Visitor::visit_f64`.
	DeVisitF64 interface {
		VisitF64(v float64) (any, error)
	}
	// DeVisitChar is `Visitor::visit_char`. The default forwards to
	// visit_str.
	DeVisitChar interface {
		VisitChar(v rune) (any, error)
	}
	// DeVisitStr is `Visitor::visit_str`: a string that lives only for the
	// call.
	DeVisitStr interface {
		VisitStr(v string) (any, error)
	}
	// DeVisitBorrowedStr is `Visitor::visit_borrowed_str`: a string that
	// lives as long as the input. The default forwards to visit_str.
	DeVisitBorrowedStr interface {
		VisitBorrowedStr(v string) (any, error)
	}
	// DeVisitString is `Visitor::visit_string`: a string the visitor may
	// keep. The default forwards to visit_str.
	DeVisitString interface {
		VisitString(v string) (any, error)
	}
	// DeVisitBytes is `Visitor::visit_bytes`: bytes that live only for the
	// call. A visitor that keeps them copies them.
	DeVisitBytes interface {
		VisitBytes(v []byte) (any, error)
	}
	// DeVisitBorrowedBytes is `Visitor::visit_borrowed_bytes`: bytes that
	// live as long as the input. The default forwards to visit_bytes.
	DeVisitBorrowedBytes interface {
		VisitBorrowedBytes(v []byte) (any, error)
	}
	// DeVisitByteBuf is `Visitor::visit_byte_buf`: bytes the visitor may
	// keep. The default forwards to visit_bytes.
	DeVisitByteBuf interface {
		VisitByteBuf(v []byte) (any, error)
	}
	// DeVisitNone is `Visitor::visit_none`.
	DeVisitNone interface {
		VisitNone() (any, error)
	}
	// DeVisitSome is `Visitor::visit_some`.
	DeVisitSome interface {
		VisitSome(deserializer Deserializer) (any, error)
	}
	// DeVisitUnit is `Visitor::visit_unit`.
	DeVisitUnit interface {
		VisitUnit() (any, error)
	}
	// DeVisitNewtypeStruct is `Visitor::visit_newtype_struct`.
	DeVisitNewtypeStruct interface {
		VisitNewtypeStruct(deserializer Deserializer) (any, error)
	}
	// DeVisitSeq is `Visitor::visit_seq`.
	DeVisitSeq interface {
		VisitSeq(seq DeSeqAccess) (any, error)
	}
	// DeVisitMap is `Visitor::visit_map`.
	DeVisitMap interface {
		VisitMap(m DeMapAccess) (any, error)
	}
	// DeVisitEnum is `Visitor::visit_enum`.
	DeVisitEnum interface {
		VisitEnum(data DeEnumAccess) (any, error)
	}
)

// DeVisitorVisitBool is `visitor.visit_bool(v)`.
func DeVisitorVisitBool(visitor DeVisitor, v bool) (any, error) {
	if own, ok := visitor.(DeVisitBool); ok {
		return own.VisitBool(v)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedBool{F0: v}, visitor)
}

// DeVisitorVisitI8 is `visitor.visit_i8(v)`.
func DeVisitorVisitI8(visitor DeVisitor, v int8) (any, error) {
	if own, ok := visitor.(DeVisitI8); ok {
		return own.VisitI8(v)
	}
	return DeVisitorVisitI64(visitor, int64(v))
}

// DeVisitorVisitI16 is `visitor.visit_i16(v)`.
func DeVisitorVisitI16(visitor DeVisitor, v int16) (any, error) {
	if own, ok := visitor.(DeVisitI16); ok {
		return own.VisitI16(v)
	}
	return DeVisitorVisitI64(visitor, int64(v))
}

// DeVisitorVisitI32 is `visitor.visit_i32(v)`.
func DeVisitorVisitI32(visitor DeVisitor, v int32) (any, error) {
	if own, ok := visitor.(DeVisitI32); ok {
		return own.VisitI32(v)
	}
	return DeVisitorVisitI64(visitor, int64(v))
}

// DeVisitorVisitI64 is `visitor.visit_i64(v)`.
func DeVisitorVisitI64(visitor DeVisitor, v int64) (any, error) {
	if own, ok := visitor.(DeVisitI64); ok {
		return own.VisitI64(v)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedSigned{F0: v}, visitor)
}

// DeVisitorVisitU8 is `visitor.visit_u8(v)`.
func DeVisitorVisitU8(visitor DeVisitor, v uint8) (any, error) {
	if own, ok := visitor.(DeVisitU8); ok {
		return own.VisitU8(v)
	}
	return DeVisitorVisitU64(visitor, uint64(v))
}

// DeVisitorVisitU16 is `visitor.visit_u16(v)`.
func DeVisitorVisitU16(visitor DeVisitor, v uint16) (any, error) {
	if own, ok := visitor.(DeVisitU16); ok {
		return own.VisitU16(v)
	}
	return DeVisitorVisitU64(visitor, uint64(v))
}

// DeVisitorVisitU32 is `visitor.visit_u32(v)`.
func DeVisitorVisitU32(visitor DeVisitor, v uint32) (any, error) {
	if own, ok := visitor.(DeVisitU32); ok {
		return own.VisitU32(v)
	}
	return DeVisitorVisitU64(visitor, uint64(v))
}

// DeVisitorVisitU64 is `visitor.visit_u64(v)`.
func DeVisitorVisitU64(visitor DeVisitor, v uint64) (any, error) {
	if own, ok := visitor.(DeVisitU64); ok {
		return own.VisitU64(v)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedUnsigned{F0: v}, visitor)
}

// DeVisitorVisitF32 is `visitor.visit_f32(v)`.
func DeVisitorVisitF32(visitor DeVisitor, v float32) (any, error) {
	if own, ok := visitor.(DeVisitF32); ok {
		return own.VisitF32(v)
	}
	return DeVisitorVisitF64(visitor, float64(v))
}

// DeVisitorVisitF64 is `visitor.visit_f64(v)`.
func DeVisitorVisitF64(visitor DeVisitor, v float64) (any, error) {
	if own, ok := visitor.(DeVisitF64); ok {
		return own.VisitF64(v)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedFloat{F0: v}, visitor)
}

// DeVisitorVisitChar is `visitor.visit_char(v)`.
func DeVisitorVisitChar(visitor DeVisitor, v rune) (any, error) {
	if own, ok := visitor.(DeVisitChar); ok {
		return own.VisitChar(v)
	}
	var buffer [utf8.UTFMax]byte
	return DeVisitorVisitStr(visitor, string(buffer[:utf8.EncodeRune(buffer[:], v)]))
}

// DeVisitorVisitStr is `visitor.visit_str(v)`.
func DeVisitorVisitStr(visitor DeVisitor, v string) (any, error) {
	if own, ok := visitor.(DeVisitStr); ok {
		return own.VisitStr(v)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedStr{F0: v}, visitor)
}

// DeVisitorVisitBorrowedStr is `visitor.visit_borrowed_str(v)`.
func DeVisitorVisitBorrowedStr(visitor DeVisitor, v string) (any, error) {
	if own, ok := visitor.(DeVisitBorrowedStr); ok {
		return own.VisitBorrowedStr(v)
	}
	return DeVisitorVisitStr(visitor, v)
}

// DeVisitorVisitString is `visitor.visit_string(v)`.
func DeVisitorVisitString(visitor DeVisitor, v string) (any, error) {
	if own, ok := visitor.(DeVisitString); ok {
		return own.VisitString(v)
	}
	return DeVisitorVisitStr(visitor, v)
}

// DeVisitorVisitBytes is `visitor.visit_bytes(v)`.
func DeVisitorVisitBytes(visitor DeVisitor, v []byte) (any, error) {
	if own, ok := visitor.(DeVisitBytes); ok {
		return own.VisitBytes(v)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedBytes{F0: v}, visitor)
}

// DeVisitorVisitBorrowedBytes is `visitor.visit_borrowed_bytes(v)`.
func DeVisitorVisitBorrowedBytes(visitor DeVisitor, v []byte) (any, error) {
	if own, ok := visitor.(DeVisitBorrowedBytes); ok {
		return own.VisitBorrowedBytes(v)
	}
	return DeVisitorVisitBytes(visitor, v)
}

// DeVisitorVisitByteBuf is `visitor.visit_byte_buf(v)`.
func DeVisitorVisitByteBuf(visitor DeVisitor, v []byte) (any, error) {
	if own, ok := visitor.(DeVisitByteBuf); ok {
		return own.VisitByteBuf(v)
	}
	return DeVisitorVisitBytes(visitor, v)
}

// DeVisitorVisitNone is `visitor.visit_none()`.
func DeVisitorVisitNone(visitor DeVisitor) (any, error) {
	if own, ok := visitor.(DeVisitNone); ok {
		return own.VisitNone()
	}
	return nil, DeErrorInvalidType(&DeUnexpectedOption{}, visitor)
}

// DeVisitorVisitSome is `visitor.visit_some(deserializer)`.
func DeVisitorVisitSome(visitor DeVisitor, deserializer Deserializer) (any, error) {
	if own, ok := visitor.(DeVisitSome); ok {
		return own.VisitSome(deserializer)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedOption{}, visitor)
}

// DeVisitorVisitUnit is `visitor.visit_unit()`.
func DeVisitorVisitUnit(visitor DeVisitor) (any, error) {
	if own, ok := visitor.(DeVisitUnit); ok {
		return own.VisitUnit()
	}
	return nil, DeErrorInvalidType(&DeUnexpectedUnit{}, visitor)
}

// DeVisitorVisitNewtypeStruct is `visitor.visit_newtype_struct(deserializer)`.
func DeVisitorVisitNewtypeStruct(visitor DeVisitor, deserializer Deserializer) (any, error) {
	if own, ok := visitor.(DeVisitNewtypeStruct); ok {
		return own.VisitNewtypeStruct(deserializer)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedNewtypeStruct{}, visitor)
}

// DeVisitorVisitSeq is `visitor.visit_seq(seq)`.
func DeVisitorVisitSeq(visitor DeVisitor, seq DeSeqAccess) (any, error) {
	if own, ok := visitor.(DeVisitSeq); ok {
		return own.VisitSeq(seq)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedSeq{}, visitor)
}

// DeVisitorVisitMap is `visitor.visit_map(map)`.
func DeVisitorVisitMap(visitor DeVisitor, m DeMapAccess) (any, error) {
	if own, ok := visitor.(DeVisitMap); ok {
		return own.VisitMap(m)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedMap{}, visitor)
}

// DeVisitorVisitEnum is `visitor.visit_enum(data)`.
func DeVisitorVisitEnum(visitor DeVisitor, data DeEnumAccess) (any, error) {
	if own, ok := visitor.(DeVisitEnum); ok {
		return own.VisitEnum(data)
	}
	return nil, DeErrorInvalidType(&DeUnexpectedEnum{}, visitor)
}

// DeDeserializeSeed is the Rust trait `de::DeserializeSeed`: the stateful
// form of Deserialize. Where Rust passes `PhantomData<T>` to get a T, Go
// passes the seed SeedFor(&value), which stores the value and returns nil.
type DeDeserializeSeed interface {
	// DeserializeSeed is `DeserializeSeed::deserialize(self, deserializer)`.
	// It returns the seed's `Value`.
	DeserializeSeed(deserializer Deserializer) (any, error)
}

// DeSeqAccess is the Rust trait `de::SeqAccess`: what a visitor reads the
// elements of a sequence from.
type DeSeqAccess interface {
	// NextElementSeed is `next_element_seed`: the next element and true, or
	// false when there are no more.
	NextElementSeed(seed DeDeserializeSeed) (value any, ok bool, err error)
	// SizeHint is `size_hint`: the number of elements remaining, if known.
	SizeHint() rs.Option[int]
}

// DeMapAccess is the Rust trait `de::MapAccess`: what a visitor reads the
// entries of a map from.
type DeMapAccess interface {
	// NextKeySeed is `next_key_seed`: the next key and true, or false when
	// there are no more entries.
	NextKeySeed(seed DeDeserializeSeed) (key any, ok bool, err error)
	// NextValueSeed is `next_value_seed`: the value of the key NextKeySeed
	// just returned.
	NextValueSeed(seed DeDeserializeSeed) (any, error)
	// NextEntrySeed is `next_entry_seed`: the next key and value and true,
	// or false when there are no more entries. An implementation with
	// nothing better to do calls DeMapAccessDefaultNextEntrySeed.
	NextEntrySeed(keySeed DeDeserializeSeed, valueSeed DeDeserializeSeed) (key any, value any, ok bool, err error)
	// SizeHint is `size_hint`: the number of entries remaining, if known.
	SizeHint() rs.Option[int]
}

// DeMapAccessDefaultNextEntrySeed is the default body of
// `MapAccess::next_entry_seed`.
func DeMapAccessDefaultNextEntrySeed(m DeMapAccess, keySeed DeDeserializeSeed, valueSeed DeDeserializeSeed) (key any, value any, ok bool, err error) {
	key, ok, err = m.NextKeySeed(keySeed)
	if err != nil || !ok {
		return nil, nil, false, err
	}
	value, err = m.NextValueSeed(valueSeed)
	if err != nil {
		return nil, nil, false, err
	}
	return key, value, true, nil
}

// DeEnumAccess is the Rust trait `de::EnumAccess`: what a visitor reads the
// variant of an enum from.
type DeEnumAccess interface {
	// VariantSeed is `variant_seed`: the variant's identifier, read with
	// seed, and the access to the variant's content.
	VariantSeed(seed DeDeserializeSeed) (any, DeVariantAccess, error)
}

// DeVariantAccess is the Rust trait `de::VariantAccess`: what a visitor
// reads the content of an enum variant from, once EnumAccess said which
// variant it is.
type DeVariantAccess interface {
	// UnitVariant is `unit_variant`: the variant holds no value.
	UnitVariant() error
	// NewtypeVariantSeed is `newtype_variant_seed`: the variant holds one
	// value.
	NewtypeVariantSeed(seed DeDeserializeSeed) (any, error)
	// TupleVariant is `tuple_variant`: the variant holds a sequence of len
	// values.
	TupleVariant(len int, visitor DeVisitor) (any, error)
	// StructVariant is `struct_variant`: the variant holds the named fields.
	StructVariant(fields []string, visitor DeVisitor) (any, error)
}
