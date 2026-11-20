// Port of serde 1.0.229 src/private/de.rs and serde_core 1.0.229
// src/private/content.rs

package serde

import (
	"math"
	"slices"
	"strconv"
	"unicode/utf8"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// This file is private and nothing here is used outside of the code that
// stands for derive-generated code: it buffers the contents of a
// Deserializer when deserializing untagged enums, internally tagged enums
// and structs with flattened fields.

type contentKind uint8

const (
	contentInvalid contentKind = iota // the zero value: never a real Content
	contentBool
	contentU8
	contentU16
	contentU32
	contentU64
	contentI8
	contentI16
	contentI32
	contentI64
	contentF32
	contentF64
	contentChar
	contentString
	contentStr
	contentByteBuf
	contentBytes
	contentNone
	contentSome
	contentUnit
	contentNewtype
	contentSeq
	contentMap
)

// content is serde's `Content<'de>`.
type content struct {
	kind contentKind
	// num holds a Bool (0 or 1), a U8..U64 (the value), an I8..I64
	// (uint64(int64(v))), an F32 (its bits), an F64 (its bits) or a Char (the
	// code point).
	num uint64
	// str holds a String or a Str.
	str string
	// bytes holds a ByteBuf or Bytes.
	bytes []byte
	// seq holds the elements of a Seq, or the one boxed content of a Some or
	// a Newtype.
	seq []content
	// entries holds a Map.
	entries []contentEntry
}

type contentEntry struct{ key, value content }

func (c *content) f32() float32 { return math.Float32frombits(uint32(c.num)) }
func (c *content) f64() float64 { return math.Float64frombits(c.num) }

// contentAsStr is `content_as_str`.
func contentAsStr(c *content) (string, bool) {
	switch c.kind {
	case contentStr, contentString:
		return c.str, true
	case contentBytes, contentByteBuf:
		if utf8.Valid(c.bytes) {
			return string(c.bytes), true
		}
	}
	return "", false
}

// contentUnexpected is `content_unexpected`.
func contentUnexpected(c *content) DeUnexpected {
	switch c.kind {
	case contentBool:
		return &DeUnexpectedBool{F0: c.num != 0}
	case contentU8, contentU16, contentU32, contentU64:
		return &DeUnexpectedUnsigned{F0: c.num}
	case contentI8, contentI16, contentI32, contentI64:
		return &DeUnexpectedSigned{F0: int64(c.num)}
	case contentF32:
		return &DeUnexpectedFloat{F0: float64(c.f32())}
	case contentF64:
		return &DeUnexpectedFloat{F0: c.f64()}
	case contentChar:
		return &DeUnexpectedChar{F0: rune(c.num)}
	case contentString, contentStr:
		return &DeUnexpectedStr{F0: c.str}
	case contentByteBuf, contentBytes:
		return &DeUnexpectedBytes{F0: c.bytes}
	case contentNone, contentSome:
		return &DeUnexpectedOption{}
	case contentUnit:
		return &DeUnexpectedUnit{}
	case contentNewtype:
		return &DeUnexpectedNewtypeStruct{}
	case contentSeq:
		return &DeUnexpectedSeq{}
	case contentMap:
		return &DeUnexpectedMap{}
	}
	panic(rs.Unreachable("serde: content of no kind"))
}

// contentVisitor is `ContentVisitor`: it builds the Content of whatever the
// deserializer holds. It stores the Content in *out and returns nil as its
// value. It is also the seed Rust writes as `ContentVisitor::new()`.
type contentVisitor struct{ out *content }

// contentSource is an override of the hidden trait method
// `Deserializer::__deserialize_content_v1`, which hands over a Content the
// deserializer already holds. A deserializer without it is asked to
// deserialize_any, as the trait's default does.
type contentSource interface {
	deserializeContentV1() content
}

func (v contentVisitor) DeserializeSeed(deserializer Deserializer) (any, error) {
	if source, ok := deserializer.(contentSource); ok {
		*v.out = source.deserializeContentV1()
		return nil, nil
	}
	return deserializer.DeserializeAny(v)
}

func (v contentVisitor) Expecting() string { return "any value" }

func (v contentVisitor) set(kind contentKind, num uint64) (any, error) {
	*v.out = content{kind: kind, num: num}
	return nil, nil
}

func (v contentVisitor) VisitBool(value bool) (any, error) {
	if value {
		return v.set(contentBool, 1)
	}
	return v.set(contentBool, 0)
}
func (v contentVisitor) VisitI8(value int8) (any, error) {
	return v.set(contentI8, uint64(int64(value)))
}
func (v contentVisitor) VisitI16(value int16) (any, error) {
	return v.set(contentI16, uint64(int64(value)))
}
func (v contentVisitor) VisitI32(value int32) (any, error) {
	return v.set(contentI32, uint64(int64(value)))
}
func (v contentVisitor) VisitI64(value int64) (any, error) { return v.set(contentI64, uint64(value)) }
func (v contentVisitor) VisitU8(value uint8) (any, error)  { return v.set(contentU8, uint64(value)) }
func (v contentVisitor) VisitU16(value uint16) (any, error) {
	return v.set(contentU16, uint64(value))
}
func (v contentVisitor) VisitU32(value uint32) (any, error) {
	return v.set(contentU32, uint64(value))
}
func (v contentVisitor) VisitU64(value uint64) (any, error) { return v.set(contentU64, value) }
func (v contentVisitor) VisitF32(value float32) (any, error) {
	return v.set(contentF32, uint64(math.Float32bits(value)))
}
func (v contentVisitor) VisitF64(value float64) (any, error) {
	return v.set(contentF64, math.Float64bits(value))
}
func (v contentVisitor) VisitChar(value rune) (any, error) { return v.set(contentChar, uint64(value)) }

func (v contentVisitor) VisitStr(value string) (any, error) {
	*v.out = content{kind: contentString, str: value}
	return nil, nil
}

func (v contentVisitor) VisitBorrowedStr(value string) (any, error) {
	*v.out = content{kind: contentStr, str: value}
	return nil, nil
}

func (v contentVisitor) VisitString(value string) (any, error) {
	*v.out = content{kind: contentString, str: value}
	return nil, nil
}

func (v contentVisitor) VisitBytes(value []byte) (any, error) {
	*v.out = content{kind: contentByteBuf, bytes: slices.Clone(value)}
	return nil, nil
}

func (v contentVisitor) VisitBorrowedBytes(value []byte) (any, error) {
	*v.out = content{kind: contentBytes, bytes: value}
	return nil, nil
}

func (v contentVisitor) VisitByteBuf(value []byte) (any, error) {
	*v.out = content{kind: contentByteBuf, bytes: value}
	return nil, nil
}

func (v contentVisitor) VisitUnit() (any, error) { return v.set(contentUnit, 0) }
func (v contentVisitor) VisitNone() (any, error) { return v.set(contentNone, 0) }

func (v contentVisitor) VisitSome(deserializer Deserializer) (any, error) {
	boxed := make([]content, 1)
	if _, err := (contentVisitor{out: &boxed[0]}).DeserializeSeed(deserializer); err != nil {
		return nil, err
	}
	*v.out = content{kind: contentSome, seq: boxed}
	return nil, nil
}

func (v contentVisitor) VisitNewtypeStruct(deserializer Deserializer) (any, error) {
	boxed := make([]content, 1)
	if _, err := (contentVisitor{out: &boxed[0]}).DeserializeSeed(deserializer); err != nil {
		return nil, err
	}
	*v.out = content{kind: contentNewtype, seq: boxed}
	return nil, nil
}

func (v contentVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	items := make([]content, 0, sizeHintCautious(seq.SizeHint(), unsafe.Sizeof(content{})))
	for {
		// The element is built in place. Nothing appends to items while it
		// is being read, so the pointer stays good.
		items = append(items, content{})
		_, ok, err := seq.NextElementSeed(contentVisitor{out: &items[len(items)-1]})
		if err != nil {
			return nil, err
		}
		if !ok {
			items = items[:len(items)-1]
			break
		}
	}
	*v.out = content{kind: contentSeq, seq: items}
	return nil, nil
}

func (v contentVisitor) VisitMap(m DeMapAccess) (any, error) {
	entries := make([]contentEntry, 0, sizeHintCautious(m.SizeHint(), unsafe.Sizeof(contentEntry{})))
	for {
		entries = append(entries, contentEntry{})
		entry := &entries[len(entries)-1]
		_, _, ok, err := m.NextEntrySeed(contentVisitor{out: &entry.key}, contentVisitor{out: &entry.value})
		if err != nil {
			return nil, err
		}
		if !ok {
			entries = entries[:len(entries)-1]
			break
		}
	}
	*v.out = content{kind: contentMap, entries: entries}
	return nil, nil
}

func (v contentVisitor) VisitEnum(DeEnumAccess) (any, error) {
	return nil, DeErrorCustom("untagged and internally tagged enums do not support enum input")
}

// tagOrContentVisitor is `TagOrContentVisitor`: the key of an entry of an
// internally tagged enum, which is either the tag or any other value. Its
// value is true for the tag; any other key is stored in *out as a Content.
//
// This is the type of the map keys in an internally tagged enum.
type tagOrContentVisitor struct {
	name string
	out  *content
}

func (v *tagOrContentVisitor) DeserializeSeed(deserializer Deserializer) (any, error) {
	// Internally tagged enums are only supported in self-describing
	// formats.
	return deserializer.DeserializeAny(v)
}

func (v *tagOrContentVisitor) Expecting() string {
	return "a type tag `" + v.name + "` or any other value"
}

func (v *tagOrContentVisitor) content() contentVisitor { return contentVisitor{out: v.out} }

func notTag(_ any, err error) (any, error) { return false, err }

func (v *tagOrContentVisitor) VisitBool(value bool) (any, error) {
	return notTag(v.content().VisitBool(value))
}
func (v *tagOrContentVisitor) VisitI8(value int8) (any, error) {
	return notTag(v.content().VisitI8(value))
}
func (v *tagOrContentVisitor) VisitI16(value int16) (any, error) {
	return notTag(v.content().VisitI16(value))
}
func (v *tagOrContentVisitor) VisitI32(value int32) (any, error) {
	return notTag(v.content().VisitI32(value))
}
func (v *tagOrContentVisitor) VisitI64(value int64) (any, error) {
	return notTag(v.content().VisitI64(value))
}
func (v *tagOrContentVisitor) VisitU8(value uint8) (any, error) {
	return notTag(v.content().VisitU8(value))
}
func (v *tagOrContentVisitor) VisitU16(value uint16) (any, error) {
	return notTag(v.content().VisitU16(value))
}
func (v *tagOrContentVisitor) VisitU32(value uint32) (any, error) {
	return notTag(v.content().VisitU32(value))
}
func (v *tagOrContentVisitor) VisitU64(value uint64) (any, error) {
	return notTag(v.content().VisitU64(value))
}
func (v *tagOrContentVisitor) VisitF32(value float32) (any, error) {
	return notTag(v.content().VisitF32(value))
}
func (v *tagOrContentVisitor) VisitF64(value float64) (any, error) {
	return notTag(v.content().VisitF64(value))
}
func (v *tagOrContentVisitor) VisitChar(value rune) (any, error) {
	return notTag(v.content().VisitChar(value))
}

func (v *tagOrContentVisitor) VisitStr(value string) (any, error) {
	if value == v.name {
		return true, nil
	}
	return notTag(v.content().VisitStr(value))
}

func (v *tagOrContentVisitor) VisitBorrowedStr(value string) (any, error) {
	if value == v.name {
		return true, nil
	}
	return notTag(v.content().VisitBorrowedStr(value))
}

func (v *tagOrContentVisitor) VisitString(value string) (any, error) {
	if value == v.name {
		return true, nil
	}
	return notTag(v.content().VisitString(value))
}

func (v *tagOrContentVisitor) VisitBytes(value []byte) (any, error) {
	if string(value) == v.name {
		return true, nil
	}
	return notTag(v.content().VisitBytes(value))
}

func (v *tagOrContentVisitor) VisitBorrowedBytes(value []byte) (any, error) {
	if string(value) == v.name {
		return true, nil
	}
	return notTag(v.content().VisitBorrowedBytes(value))
}

func (v *tagOrContentVisitor) VisitByteBuf(value []byte) (any, error) {
	if string(value) == v.name {
		return true, nil
	}
	return notTag(v.content().VisitByteBuf(value))
}

func (v *tagOrContentVisitor) VisitUnit() (any, error) { return notTag(v.content().VisitUnit()) }
func (v *tagOrContentVisitor) VisitNone() (any, error) { return notTag(v.content().VisitNone()) }

func (v *tagOrContentVisitor) VisitSome(deserializer Deserializer) (any, error) {
	return notTag(v.content().VisitSome(deserializer))
}

func (v *tagOrContentVisitor) VisitNewtypeStruct(deserializer Deserializer) (any, error) {
	return notTag(v.content().VisitNewtypeStruct(deserializer))
}

func (v *tagOrContentVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	return notTag(v.content().VisitSeq(seq))
}

func (v *tagOrContentVisitor) VisitMap(m DeMapAccess) (any, error) {
	return notTag(v.content().VisitMap(m))
}

func (v *tagOrContentVisitor) VisitEnum(data DeEnumAccess) (any, error) {
	return notTag(v.content().VisitEnum(data))
}

// taggedContentVisitor is `TaggedContentVisitor<T>`, used by generated code
// to deserialize an internally tagged enum.
//
// It captures a map or sequence from the original deserializer and searches
// a tag in it (in case of a sequence, the tag is the first element of the
// sequence). Rust's value is `(T, Content)`: tagSeed reads the T, which is
// the variant identifier, and keeps it; the Content is stored in *out.
type taggedContentVisitor struct {
	tagName   string
	expecting string
	tagSeed   DeDeserializeSeed
	out       *content
}

func (v *taggedContentVisitor) Expecting() string { return v.expecting }

func (v *taggedContentVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	_, ok, err := seq.NextElementSeed(v.tagSeed)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, DeErrorMissingField(v.tagName)
	}
	// Rust wraps the rest of the sequence in a SeqAccessDeserializer and
	// gives that to ContentVisitor::deserialize, which comes to visit_seq.
	return contentVisitor{out: v.out}.VisitSeq(seq)
}

func (v *taggedContentVisitor) VisitMap(m DeMapAccess) (any, error) {
	tagSeen := false
	entries := make([]contentEntry, 0, sizeHintCautious(m.SizeHint(), unsafe.Sizeof(contentEntry{})))
	keyVisitor := &tagOrContentVisitor{name: v.tagName}
	for {
		entries = append(entries, contentEntry{})
		entry := &entries[len(entries)-1]
		keyVisitor.out = &entry.key
		isTag, ok, err := m.NextKeySeed(keyVisitor)
		if err != nil {
			return nil, err
		}
		if !ok {
			entries = entries[:len(entries)-1]
			break
		}
		if isTag.(bool) {
			entries = entries[:len(entries)-1]
			if tagSeen {
				return nil, DeErrorDuplicateField(v.tagName)
			}
			if _, err := m.NextValueSeed(v.tagSeed); err != nil {
				return nil, err
			}
			tagSeen = true
			continue
		}
		if _, err := m.NextValueSeed(contentVisitor{out: &entry.value}); err != nil {
			return nil, err
		}
	}
	if !tagSeen {
		return nil, DeErrorMissingField(v.tagName)
	}
	*v.out = content{kind: contentMap, entries: entries}
	return nil, nil
}

// tagOrContentField is `TagOrContentField`, used by generated code to
// deserialize an adjacently tagged enum.
type tagOrContentField uint8

const (
	tagOrContentFieldTag tagOrContentField = iota
	tagOrContentFieldContent
)

// tagOrContentFieldVisitor is `TagOrContentFieldVisitor`. Its value is a
// tagOrContentField.
type tagOrContentFieldVisitor struct {
	// tag is the name of the tag field of the adjacently tagged enum.
	tag string
	// content is the name of the content field of the adjacently tagged
	// enum.
	content string
}

func (v *tagOrContentFieldVisitor) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIdentifier(v)
}

func (v *tagOrContentFieldVisitor) Expecting() string {
	return DebugStr(v.tag) + " or " + DebugStr(v.content)
}

func (v *tagOrContentFieldVisitor) VisitU64(fieldIndex uint64) (any, error) {
	switch fieldIndex {
	case 0:
		return tagOrContentFieldTag, nil
	case 1:
		return tagOrContentFieldContent, nil
	}
	return nil, DeErrorInvalidValue(&DeUnexpectedUnsigned{F0: fieldIndex}, v)
}

func (v *tagOrContentFieldVisitor) VisitStr(field string) (any, error) {
	switch field {
	case v.tag:
		return tagOrContentFieldTag, nil
	case v.content:
		return tagOrContentFieldContent, nil
	}
	return nil, DeErrorInvalidValue(&DeUnexpectedStr{F0: field}, v)
}

func (v *tagOrContentFieldVisitor) VisitBytes(field []byte) (any, error) {
	switch string(field) {
	case v.tag:
		return tagOrContentFieldTag, nil
	case v.content:
		return tagOrContentFieldContent, nil
	}
	return nil, DeErrorInvalidValue(&DeUnexpectedBytes{F0: field}, v)
}

// tagContentOtherField is `TagContentOtherField`, used by generated code to
// deserialize an adjacently tagged enum when ignoring unrelated fields is
// allowed.
type tagContentOtherField uint8

const (
	tagContentOtherFieldTag tagContentOtherField = iota
	tagContentOtherFieldContent
	tagContentOtherFieldOther
)

// tagContentOtherFieldVisitor is `TagContentOtherFieldVisitor`. Its value is
// a tagContentOtherField.
type tagContentOtherFieldVisitor struct {
	// tag is the name of the tag field of the adjacently tagged enum.
	tag string
	// content is the name of the content field of the adjacently tagged
	// enum.
	content string
}

func (v *tagContentOtherFieldVisitor) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIdentifier(v)
}

func (v *tagContentOtherFieldVisitor) Expecting() string {
	return DebugStr(v.tag) + ", " + DebugStr(v.content) + ", or other ignored fields"
}

func (v *tagContentOtherFieldVisitor) VisitU64(fieldIndex uint64) (any, error) {
	switch fieldIndex {
	case 0:
		return tagContentOtherFieldTag, nil
	case 1:
		return tagContentOtherFieldContent, nil
	}
	return tagContentOtherFieldOther, nil
}

func (v *tagContentOtherFieldVisitor) VisitStr(field string) (any, error) {
	switch field {
	case v.tag:
		return tagContentOtherFieldTag, nil
	case v.content:
		return tagContentOtherFieldContent, nil
	}
	return tagContentOtherFieldOther, nil
}

func (v *tagContentOtherFieldVisitor) VisitBytes(field []byte) (any, error) {
	return v.VisitStr(string(field))
}

// contentDeserializer is `ContentDeserializer`, which owns the content it
// reads, and contentRefDeserializer is `ContentRefDeserializer`, which
// borrows it. Both are a *content under another name, so making one
// allocates nothing.
//
// Used by generated code to deserialize the content of untagged and
// internally tagged enums.
type (
	contentDeserializer    content
	contentRefDeserializer content
)

func newContentDeserializer(c *content) *contentDeserializer {
	return (*contentDeserializer)(c)
}

func newContentRefDeserializer(c *content) *contentRefDeserializer {
	return (*contentRefDeserializer)(c)
}

// contentDe is the body of both: owned says which one is running.
type contentDe struct {
	c     *content
	owned bool
}

func (d contentDe) deserializer() Deserializer { return contentDeserializerOf(d.c, d.owned) }

func contentDeserializerOf(c *content, owned bool) Deserializer {
	if owned {
		return (*contentDeserializer)(c)
	}
	return (*contentRefDeserializer)(c)
}

func (d contentDe) invalidType(exp DeExpected) error {
	return DeErrorInvalidType(contentUnexpected(d.c), exp)
}

// visitStr gives the visitor a String or Str content.
func (d contentDe) visitStr(visitor DeVisitor) (any, error) {
	if d.c.kind == contentStr {
		return DeVisitorVisitBorrowedStr(visitor, d.c.str)
	}
	if d.owned {
		return DeVisitorVisitString(visitor, d.c.str)
	}
	return DeVisitorVisitStr(visitor, d.c.str)
}

// visitBytes gives the visitor a ByteBuf or Bytes content.
func (d contentDe) visitBytes(visitor DeVisitor) (any, error) {
	if d.c.kind == contentBytes {
		return DeVisitorVisitBorrowedBytes(visitor, d.c.bytes)
	}
	if d.owned {
		return DeVisitorVisitByteBuf(visitor, d.c.bytes)
	}
	return DeVisitorVisitBytes(visitor, d.c.bytes)
}

func (d contentDe) visitInteger(visitor DeVisitor) (any, error, bool) {
	c := d.c
	switch c.kind {
	case contentU8:
		value, err := DeVisitorVisitU8(visitor, uint8(c.num))
		return value, err, true
	case contentU16:
		value, err := DeVisitorVisitU16(visitor, uint16(c.num))
		return value, err, true
	case contentU32:
		value, err := DeVisitorVisitU32(visitor, uint32(c.num))
		return value, err, true
	case contentU64:
		value, err := DeVisitorVisitU64(visitor, c.num)
		return value, err, true
	case contentI8:
		value, err := DeVisitorVisitI8(visitor, int8(c.num))
		return value, err, true
	case contentI16:
		value, err := DeVisitorVisitI16(visitor, int16(c.num))
		return value, err, true
	case contentI32:
		value, err := DeVisitorVisitI32(visitor, int32(c.num))
		return value, err, true
	case contentI64:
		value, err := DeVisitorVisitI64(visitor, int64(c.num))
		return value, err, true
	}
	return nil, nil, false
}

func (d contentDe) integer(visitor DeVisitor) (any, error) {
	if value, err, ok := d.visitInteger(visitor); ok {
		return value, err
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) float(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentF32:
		return DeVisitorVisitF32(visitor, d.c.f32())
	case contentF64:
		return DeVisitorVisitF64(visitor, d.c.f64())
	}
	return d.integer(visitor)
}

func (d contentDe) boxed() Deserializer { return contentDeserializerOf(&d.c.seq[0], d.owned) }

func (d contentDe) any(visitor DeVisitor) (any, error) {
	c := d.c
	switch c.kind {
	case contentBool:
		return DeVisitorVisitBool(visitor, c.num != 0)
	case contentU8, contentU16, contentU32, contentU64, contentI8, contentI16, contentI32, contentI64:
		value, err, _ := d.visitInteger(visitor)
		return value, err
	case contentF32:
		return DeVisitorVisitF32(visitor, c.f32())
	case contentF64:
		return DeVisitorVisitF64(visitor, c.f64())
	case contentChar:
		return DeVisitorVisitChar(visitor, rune(c.num))
	case contentString, contentStr:
		return d.visitStr(visitor)
	case contentByteBuf, contentBytes:
		return d.visitBytes(visitor)
	case contentUnit:
		return DeVisitorVisitUnit(visitor)
	case contentNone:
		return DeVisitorVisitNone(visitor)
	case contentSome:
		return DeVisitorVisitSome(visitor, d.boxed())
	case contentNewtype:
		return DeVisitorVisitNewtypeStruct(visitor, d.boxed())
	case contentSeq:
		return visitContentSeq(c.seq, d.owned, visitor)
	case contentMap:
		return visitContentMap(c.entries, d.owned, visitor)
	}
	panic(rs.Unreachable("serde: content of no kind"))
}

func (d contentDe) bool(visitor DeVisitor) (any, error) {
	if d.c.kind == contentBool {
		return DeVisitorVisitBool(visitor, d.c.num != 0)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) char(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentChar:
		return DeVisitorVisitChar(visitor, rune(d.c.num))
	case contentString, contentStr:
		return d.visitStr(visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) str(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentString, contentStr:
		return d.visitStr(visitor)
	case contentByteBuf, contentBytes:
		return d.visitBytes(visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) byteBuf(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentString, contentStr:
		return d.visitStr(visitor)
	case contentByteBuf, contentBytes:
		return d.visitBytes(visitor)
	case contentSeq:
		return visitContentSeq(d.c.seq, d.owned, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) option(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentNone:
		return DeVisitorVisitNone(visitor)
	case contentSome:
		return DeVisitorVisitSome(visitor, d.boxed())
	case contentUnit:
		return DeVisitorVisitUnit(visitor)
	}
	// This case is to support data formats which do not encode an
	// indication whether a value is optional. An example of such a format is
	// JSON, and a counterexample is RON. When requesting `deserialize_any`
	// in JSON, the data format never performs `Visitor::visit_some` but we
	// still must be able to deserialize the resulting Content into data
	// structures with optional fields.
	return DeVisitorVisitSome(visitor, d.deserializer())
}

func (d contentDe) unit(visitor DeVisitor) (any, error) {
	switch {
	case d.c.kind == contentUnit:
		return DeVisitorVisitUnit(visitor)
	// Allow deserializing newtype variant containing unit.
	//
	//     #[derive(Deserialize)]
	//     #[serde(tag = "result")]
	//     enum Response<T> {
	//         Success(T),
	//     }
	//
	// We want {"result":"Success"} to deserialize into Response<()>.
	case d.owned && d.c.kind == contentMap && len(d.c.entries) == 0:
		return DeVisitorVisitUnit(visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) unitStruct(visitor DeVisitor) (any, error) {
	if !d.owned {
		return d.unit(visitor)
	}
	// As a special case, allow deserializing untagged newtype variant
	// containing unit struct.
	//
	//     #[derive(Deserialize)]
	//     struct Info;
	//
	//     #[derive(Deserialize)]
	//     #[serde(tag = "topic")]
	//     enum Message {
	//         Info(Info),
	//     }
	//
	// We want {"topic":"Info"} to deserialize even though ordinarily unit
	// structs do not deserialize from empty map/seq.
	switch {
	case d.c.kind == contentMap && len(d.c.entries) == 0:
		return DeVisitorVisitUnit(visitor)
	case d.c.kind == contentSeq && len(d.c.seq) == 0:
		return DeVisitorVisitUnit(visitor)
	}
	return d.any(visitor)
}

func (d contentDe) newtypeStruct(visitor DeVisitor) (any, error) {
	if d.c.kind == contentNewtype {
		return DeVisitorVisitNewtypeStruct(visitor, d.boxed())
	}
	// This case is to support data formats that encode newtype structs and
	// their underlying data the same, with no indication whether a newtype
	// wrapper was present. For example JSON does this, while RON does not.
	// In RON a newtype's name is included in the serialized representation
	// and it knows to call `Visitor::visit_newtype_struct` from
	// `deserialize_any`. JSON's `deserialize_any` never calls
	// `visit_newtype_struct` but in this code we still must be able to
	// deserialize the resulting Content into newtypes.
	return DeVisitorVisitNewtypeStruct(visitor, d.deserializer())
}

func (d contentDe) seq(visitor DeVisitor) (any, error) {
	if d.c.kind == contentSeq {
		return visitContentSeq(d.c.seq, d.owned, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) mapping(visitor DeVisitor) (any, error) {
	if d.c.kind == contentMap {
		return visitContentMap(d.c.entries, d.owned, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) structure(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentSeq:
		return visitContentSeq(d.c.seq, d.owned, visitor)
	case contentMap:
		return visitContentMap(d.c.entries, d.owned, visitor)
	}
	return nil, d.invalidType(visitor)
}

func (d contentDe) enum(visitor DeVisitor) (any, error) {
	var variant, value *content
	switch d.c.kind {
	case contentMap:
		// enums are encoded in json as maps with a single key:value pair
		if len(d.c.entries) != 1 {
			return nil, DeErrorInvalidValue(&DeUnexpectedMap{}, DeExpectedStr("map with a single key"))
		}
		variant, value = &d.c.entries[0].key, &d.c.entries[0].value
	case contentString, contentStr:
		variant = d.c
	default:
		return nil, DeErrorInvalidType(contentUnexpected(d.c), DeExpectedStr("string or map"))
	}
	return DeVisitorVisitEnum(visitor, &contentEnumDeserializer{variant: variant, value: value, owned: d.owned})
}

func (d contentDe) identifier(visitor DeVisitor) (any, error) {
	switch d.c.kind {
	case contentString, contentStr:
		return d.visitStr(visitor)
	case contentByteBuf, contentBytes:
		return d.visitBytes(visitor)
	case contentU8:
		return DeVisitorVisitU8(visitor, uint8(d.c.num))
	case contentU64:
		return DeVisitorVisitU64(visitor, d.c.num)
	}
	return nil, d.invalidType(visitor)
}

func (d *contentDeserializer) de() contentDe { return contentDe{c: (*content)(d), owned: true} }

func (d *contentDeserializer) deserializeContentV1() content { return *(*content)(d) }

func (d *contentDeserializer) DeserializeAny(v DeVisitor) (any, error)     { return d.de().any(v) }
func (d *contentDeserializer) DeserializeBool(v DeVisitor) (any, error)    { return d.de().bool(v) }
func (d *contentDeserializer) DeserializeI8(v DeVisitor) (any, error)      { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeI16(v DeVisitor) (any, error)     { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeI32(v DeVisitor) (any, error)     { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeI64(v DeVisitor) (any, error)     { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeU8(v DeVisitor) (any, error)      { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeU16(v DeVisitor) (any, error)     { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeU32(v DeVisitor) (any, error)     { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeU64(v DeVisitor) (any, error)     { return d.de().integer(v) }
func (d *contentDeserializer) DeserializeF32(v DeVisitor) (any, error)     { return d.de().float(v) }
func (d *contentDeserializer) DeserializeF64(v DeVisitor) (any, error)     { return d.de().float(v) }
func (d *contentDeserializer) DeserializeChar(v DeVisitor) (any, error)    { return d.de().char(v) }
func (d *contentDeserializer) DeserializeStr(v DeVisitor) (any, error)     { return d.de().str(v) }
func (d *contentDeserializer) DeserializeString(v DeVisitor) (any, error)  { return d.de().str(v) }
func (d *contentDeserializer) DeserializeBytes(v DeVisitor) (any, error)   { return d.de().byteBuf(v) }
func (d *contentDeserializer) DeserializeByteBuf(v DeVisitor) (any, error) { return d.de().byteBuf(v) }
func (d *contentDeserializer) DeserializeOption(v DeVisitor) (any, error)  { return d.de().option(v) }
func (d *contentDeserializer) DeserializeUnit(v DeVisitor) (any, error)    { return d.de().unit(v) }
func (d *contentDeserializer) DeserializeUnitStruct(_ string, v DeVisitor) (any, error) {
	return d.de().unitStruct(v)
}
func (d *contentDeserializer) DeserializeNewtypeStruct(_ string, v DeVisitor) (any, error) {
	return d.de().newtypeStruct(v)
}
func (d *contentDeserializer) DeserializeSeq(v DeVisitor) (any, error)          { return d.de().seq(v) }
func (d *contentDeserializer) DeserializeTuple(_ int, v DeVisitor) (any, error) { return d.de().seq(v) }
func (d *contentDeserializer) DeserializeTupleStruct(_ string, _ int, v DeVisitor) (any, error) {
	return d.de().seq(v)
}
func (d *contentDeserializer) DeserializeMap(v DeVisitor) (any, error) { return d.de().mapping(v) }
func (d *contentDeserializer) DeserializeStruct(_ string, _ []string, v DeVisitor) (any, error) {
	return d.de().structure(v)
}
func (d *contentDeserializer) DeserializeEnum(_ string, _ []string, v DeVisitor) (any, error) {
	return d.de().enum(v)
}
func (d *contentDeserializer) DeserializeIdentifier(v DeVisitor) (any, error) {
	return d.de().identifier(v)
}
func (d *contentDeserializer) DeserializeIgnoredAny(v DeVisitor) (any, error) {
	return DeVisitorVisitUnit(v)
}
func (d *contentDeserializer) IsHumanReadable() bool { return true }

func (d *contentRefDeserializer) de() contentDe { return contentDe{c: (*content)(d), owned: false} }

// deserializeContentV1 clones the content in Rust. Go shares it: a Content
// is never changed once it is built.
func (d *contentRefDeserializer) deserializeContentV1() content { return *(*content)(d) }

func (d *contentRefDeserializer) DeserializeAny(v DeVisitor) (any, error)  { return d.de().any(v) }
func (d *contentRefDeserializer) DeserializeBool(v DeVisitor) (any, error) { return d.de().bool(v) }
func (d *contentRefDeserializer) DeserializeI8(v DeVisitor) (any, error)   { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeI16(v DeVisitor) (any, error)  { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeI32(v DeVisitor) (any, error)  { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeI64(v DeVisitor) (any, error)  { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeU8(v DeVisitor) (any, error)   { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeU16(v DeVisitor) (any, error)  { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeU32(v DeVisitor) (any, error)  { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeU64(v DeVisitor) (any, error)  { return d.de().integer(v) }
func (d *contentRefDeserializer) DeserializeF32(v DeVisitor) (any, error)  { return d.de().float(v) }
func (d *contentRefDeserializer) DeserializeF64(v DeVisitor) (any, error)  { return d.de().float(v) }
func (d *contentRefDeserializer) DeserializeChar(v DeVisitor) (any, error) { return d.de().char(v) }
func (d *contentRefDeserializer) DeserializeStr(v DeVisitor) (any, error)  { return d.de().str(v) }
func (d *contentRefDeserializer) DeserializeString(v DeVisitor) (any, error) {
	return d.de().str(v)
}
func (d *contentRefDeserializer) DeserializeBytes(v DeVisitor) (any, error) {
	return d.de().byteBuf(v)
}
func (d *contentRefDeserializer) DeserializeByteBuf(v DeVisitor) (any, error) {
	return d.de().byteBuf(v)
}
func (d *contentRefDeserializer) DeserializeOption(v DeVisitor) (any, error) {
	return d.de().option(v)
}
func (d *contentRefDeserializer) DeserializeUnit(v DeVisitor) (any, error) { return d.de().unit(v) }
func (d *contentRefDeserializer) DeserializeUnitStruct(_ string, v DeVisitor) (any, error) {
	return d.de().unitStruct(v)
}
func (d *contentRefDeserializer) DeserializeNewtypeStruct(_ string, v DeVisitor) (any, error) {
	return d.de().newtypeStruct(v)
}
func (d *contentRefDeserializer) DeserializeSeq(v DeVisitor) (any, error) { return d.de().seq(v) }
func (d *contentRefDeserializer) DeserializeTuple(_ int, v DeVisitor) (any, error) {
	return d.de().seq(v)
}
func (d *contentRefDeserializer) DeserializeTupleStruct(_ string, _ int, v DeVisitor) (any, error) {
	return d.de().seq(v)
}
func (d *contentRefDeserializer) DeserializeMap(v DeVisitor) (any, error) { return d.de().mapping(v) }
func (d *contentRefDeserializer) DeserializeStruct(_ string, _ []string, v DeVisitor) (any, error) {
	return d.de().structure(v)
}
func (d *contentRefDeserializer) DeserializeEnum(_ string, _ []string, v DeVisitor) (any, error) {
	return d.de().enum(v)
}
func (d *contentRefDeserializer) DeserializeIdentifier(v DeVisitor) (any, error) {
	return d.de().identifier(v)
}
func (d *contentRefDeserializer) DeserializeIgnoredAny(v DeVisitor) (any, error) {
	return DeVisitorVisitUnit(v)
}
func (d *contentRefDeserializer) IsHumanReadable() bool { return true }

// visitContentSeq is `visit_content_seq` and `visit_content_seq_ref`, and
// `SeqDeserializer::deserialize_any`: the visitor reads the elements, and
// any it leaves unread are an error.
func visitContentSeq(items []content, owned bool, visitor DeVisitor) (any, error) {
	seq := &contentSeqAccess{items: items, owned: owned}
	value, err := DeVisitorVisitSeq(visitor, seq)
	if err != nil {
		return nil, err
	}
	if err := seq.end(); err != nil {
		return nil, err
	}
	return value, nil
}

// visitContentMap is `visit_content_map` and `visit_content_map_ref`, and
// `MapDeserializer::deserialize_any`.
func visitContentMap(entries []contentEntry, owned bool, visitor DeVisitor) (any, error) {
	m := &contentMapAccess{entries: entries, owned: owned}
	value, err := DeVisitorVisitMap(visitor, m)
	if err != nil {
		return nil, err
	}
	if err := m.end(); err != nil {
		return nil, err
	}
	return value, nil
}

// contentSeqAccess is the SeqAccess of the private `SeqDeserializer` and
// `SeqRefDeserializer`.
type contentSeqAccess struct {
	items []content
	count int
	owned bool
}

func (s *contentSeqAccess) end() error {
	remaining := len(s.items) - s.count
	if remaining == 0 {
		return nil
	}
	// First argument is the number of elements in the data, second argument
	// is the number of elements expected by the Deserialize.
	return DeErrorInvalidLength(s.count+remaining, expectedInSeq(s.count))
}

func (s *contentSeqAccess) NextElementSeed(seed DeDeserializeSeed) (any, bool, error) {
	if s.count >= len(s.items) {
		return nil, false, nil
	}
	item := &s.items[s.count]
	s.count++
	value, err := seed.DeserializeSeed(contentDeserializerOf(item, s.owned))
	if err != nil {
		return nil, false, err
	}
	return value, true, nil
}

func (s *contentSeqAccess) SizeHint() rs.Option[int] { return rs.Some(len(s.items) - s.count) }

// expectedInSeq is `ExpectedInSeq`.
type expectedInSeq int

func (e expectedInSeq) Expecting() string {
	if e == 1 {
		return "1 element in sequence"
	}
	return strconv.Itoa(int(e)) + " elements in sequence"
}

// expectedInMap is `ExpectedInMap`.
type expectedInMap int

func (e expectedInMap) Expecting() string {
	if e == 1 {
		return "1 element in map"
	}
	return strconv.Itoa(int(e)) + " elements in map"
}

// contentMapAccess is the MapAccess of the private `MapDeserializer` and
// `MapRefDeserializer`. (They are also a SeqAccess over their pairs, which
// only their own deserialize_seq uses and nothing here calls.)
type contentMapAccess struct {
	entries []contentEntry
	count   int
	value   *content
	owned   bool
}

func (m *contentMapAccess) end() error {
	remaining := len(m.entries) - m.count
	if remaining == 0 {
		return nil
	}
	// First argument is the number of elements in the data, second argument
	// is the number of elements expected by the Deserialize.
	return DeErrorInvalidLength(m.count+remaining, expectedInMap(m.count))
}

func (m *contentMapAccess) nextPair() *contentEntry {
	if m.count >= len(m.entries) {
		return nil
	}
	entry := &m.entries[m.count]
	m.count++
	return entry
}

func (m *contentMapAccess) NextKeySeed(seed DeDeserializeSeed) (any, bool, error) {
	entry := m.nextPair()
	if entry == nil {
		return nil, false, nil
	}
	m.value = &entry.value
	key, err := seed.DeserializeSeed(contentDeserializerOf(&entry.key, m.owned))
	if err != nil {
		return nil, false, err
	}
	return key, true, nil
}

func (m *contentMapAccess) NextValueSeed(seed DeDeserializeSeed) (any, error) {
	value := m.value
	m.value = nil
	// Panic because this indicates a bug in the program rather than an
	// expected failure.
	if value == nil {
		panic(rs.Panicf("MapAccess::next_value called before next_key"))
	}
	return seed.DeserializeSeed(contentDeserializerOf(value, m.owned))
}

func (m *contentMapAccess) NextEntrySeed(keySeed DeDeserializeSeed, valueSeed DeDeserializeSeed) (any, any, bool, error) {
	entry := m.nextPair()
	if entry == nil {
		return nil, nil, false, nil
	}
	key, err := keySeed.DeserializeSeed(contentDeserializerOf(&entry.key, m.owned))
	if err != nil {
		return nil, nil, false, err
	}
	value, err := valueSeed.DeserializeSeed(contentDeserializerOf(&entry.value, m.owned))
	if err != nil {
		return nil, nil, false, err
	}
	return key, value, true, nil
}

func (m *contentMapAccess) SizeHint() rs.Option[int] { return rs.Some(len(m.entries) - m.count) }

// contentEnumDeserializer is `EnumDeserializer` and `EnumRefDeserializer`,
// and the `VariantDeserializer` and `VariantRefDeserializer` they return.
// value is nil for a variant written as a bare string.
type contentEnumDeserializer struct {
	variant *content
	value   *content
	owned   bool
}

func (e *contentEnumDeserializer) VariantSeed(seed DeDeserializeSeed) (any, DeVariantAccess, error) {
	value, err := seed.DeserializeSeed(contentDeserializerOf(e.variant, e.owned))
	if err != nil {
		return nil, nil, err
	}
	return value, e, nil
}

func (e *contentEnumDeserializer) UnitVariant() error {
	if e.value == nil {
		return nil
	}
	_, err := contentDeserializerOf(e.value, e.owned).DeserializeUnit(unitVisitor{})
	return err
}

func (e *contentEnumDeserializer) NewtypeVariantSeed(seed DeDeserializeSeed) (any, error) {
	if e.value == nil {
		return nil, DeErrorInvalidType(&DeUnexpectedUnitVariant{}, DeExpectedStr("newtype variant"))
	}
	return seed.DeserializeSeed(contentDeserializerOf(e.value, e.owned))
}

func (e *contentEnumDeserializer) TupleVariant(_ int, visitor DeVisitor) (any, error) {
	switch {
	case e.value == nil:
		return nil, DeErrorInvalidType(&DeUnexpectedUnitVariant{}, DeExpectedStr("tuple variant"))
	case e.value.kind == contentSeq:
		return visitContentSeq(e.value.seq, e.owned, visitor)
	}
	return nil, DeErrorInvalidType(contentUnexpected(e.value), DeExpectedStr("tuple variant"))
}

func (e *contentEnumDeserializer) StructVariant(_ []string, visitor DeVisitor) (any, error) {
	switch {
	case e.value == nil:
		return nil, DeErrorInvalidType(&DeUnexpectedUnitVariant{}, DeExpectedStr("struct variant"))
	case e.value.kind == contentMap:
		return visitContentMap(e.value.entries, e.owned, visitor)
	case e.value.kind == contentSeq:
		return visitContentSeq(e.value.seq, e.owned, visitor)
	}
	return nil, DeErrorInvalidType(contentUnexpected(e.value), DeExpectedStr("struct variant"))
}

// unitVisitor is the visitor of `impl Deserialize for ()`.
type unitVisitor struct{}

func (unitVisitor) Expecting() string       { return "unit" }
func (unitVisitor) VisitUnit() (any, error) { return nil, nil }

// internallyTaggedUnitVisitor is `InternallyTaggedUnitVisitor`: it visits
// an internally tagged unit variant.
type internallyTaggedUnitVisitor struct {
	typeName    string
	variantName string
}

func (v *internallyTaggedUnitVisitor) Expecting() string {
	return "unit variant " + v.typeName + "::" + v.variantName
}

func (v *internallyTaggedUnitVisitor) VisitSeq(DeSeqAccess) (any, error) { return nil, nil }

func (v *internallyTaggedUnitVisitor) VisitMap(m DeMapAccess) (any, error) {
	for {
		_, _, ok, err := m.NextEntrySeed(DeIgnoredAny{}, DeIgnoredAny{})
		if err != nil || !ok {
			return nil, err
		}
	}
}

// untaggedUnitVisitor is `UntaggedUnitVisitor`: it visits an untagged unit
// variant.
type untaggedUnitVisitor struct {
	typeName    string
	variantName string
}

func (v *untaggedUnitVisitor) Expecting() string {
	return "unit variant " + v.typeName + "::" + v.variantName
}

func (v *untaggedUnitVisitor) VisitUnit() (any, error) { return nil, nil }
func (v *untaggedUnitVisitor) VisitNone() (any, error) { return nil, nil }

// flatMapEntry is one element of the `Vec<Option<(Content, Content)>>` that
// a struct with flattened fields collects the entries it does not know
// into. present is false once a flattened field has claimed the entry
// (Rust's None).
type flatMapEntry struct {
	present bool
	key     content
	value   content
}

// untaggedOptionVisitor is an override of the hidden trait method
// `Visitor::__private_visit_untagged_option`, which only the visitor of an
// Option has: a flattened Option is None when its content does not
// deserialize.
type untaggedOptionVisitor interface {
	privateVisitUntaggedOption(deserializer Deserializer) (any, error)
}

// flatMapDeserializer is `FlatMapDeserializer`: what a flattened field is
// deserialized from.
type flatMapDeserializer struct{ entries *[]flatMapEntry }

func flatMapDeserializeOther() (any, error) {
	return nil, DeErrorCustom("can only flatten structs and maps")
}

func (d flatMapDeserializer) DeserializeAny(visitor DeVisitor) (any, error) {
	return d.DeserializeMap(visitor)
}

func (d flatMapDeserializer) DeserializeEnum(name string, variants []string, visitor DeVisitor) (any, error) {
	for index := range *d.entries {
		if entry := flatMapTakeEntry(&(*d.entries)[index], variants); entry != nil {
			return DeVisitorVisitEnum(visitor, &contentEnumDeserializer{variant: &entry.key, value: &entry.value, owned: true})
		}
	}
	return nil, DeErrorCustom("no variant of enum " + name + " found in flattened data")
}

func (d flatMapDeserializer) DeserializeMap(visitor DeVisitor) (any, error) {
	return DeVisitorVisitMap(visitor, &flatMapAccess{entries: *d.entries})
}

func (d flatMapDeserializer) DeserializeStruct(_ string, fields []string, visitor DeVisitor) (any, error) {
	return DeVisitorVisitMap(visitor, &flatStructAccess{entries: *d.entries, fields: fields})
}

func (d flatMapDeserializer) DeserializeNewtypeStruct(_ string, visitor DeVisitor) (any, error) {
	return DeVisitorVisitNewtypeStruct(visitor, d)
}

func (d flatMapDeserializer) DeserializeOption(visitor DeVisitor) (any, error) {
	if own, ok := visitor.(untaggedOptionVisitor); ok {
		return own.privateVisitUntaggedOption(d)
	}
	return flatMapDeserializeOther()
}

func (d flatMapDeserializer) DeserializeUnit(visitor DeVisitor) (any, error) {
	return DeVisitorVisitUnit(visitor)
}

func (d flatMapDeserializer) DeserializeUnitStruct(_ string, visitor DeVisitor) (any, error) {
	return DeVisitorVisitUnit(visitor)
}

func (d flatMapDeserializer) DeserializeIgnoredAny(visitor DeVisitor) (any, error) {
	return DeVisitorVisitUnit(visitor)
}

func (flatMapDeserializer) DeserializeBool(DeVisitor) (any, error) { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeI8(DeVisitor) (any, error)   { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeI16(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeI32(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeI64(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeU8(DeVisitor) (any, error)   { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeU16(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeU32(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeU64(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeF32(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeF64(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeChar(DeVisitor) (any, error) { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeStr(DeVisitor) (any, error)  { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeString(DeVisitor) (any, error) {
	return flatMapDeserializeOther()
}
func (flatMapDeserializer) DeserializeBytes(DeVisitor) (any, error) { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeByteBuf(DeVisitor) (any, error) {
	return flatMapDeserializeOther()
}
func (flatMapDeserializer) DeserializeSeq(DeVisitor) (any, error) { return flatMapDeserializeOther() }
func (flatMapDeserializer) DeserializeTuple(int, DeVisitor) (any, error) {
	return flatMapDeserializeOther()
}
func (flatMapDeserializer) DeserializeTupleStruct(string, int, DeVisitor) (any, error) {
	return flatMapDeserializeOther()
}
func (flatMapDeserializer) DeserializeIdentifier(DeVisitor) (any, error) {
	return flatMapDeserializeOther()
}
func (flatMapDeserializer) IsHumanReadable() bool { return true }

// flatMapAccess is `FlatMapAccess`: a flattened map sees every entry still
// present.
type flatMapAccess struct {
	entries []flatMapEntry
	next    int
	pending *content
}

func (a *flatMapAccess) NextKeySeed(seed DeDeserializeSeed) (any, bool, error) {
	for a.next < len(a.entries) {
		item := &a.entries[a.next]
		a.next++
		// Items in the vector are nulled out when used by a struct.
		if item.present {
			// Do not take(), instead borrow this entry. The internally
			// tagged enum does its own buffering so we can't tell whether
			// this entry is going to be consumed. Borrowing here leaves the
			// entry available for later flattened fields.
			a.pending = &item.value
			key, err := seed.DeserializeSeed(newContentRefDeserializer(&item.key))
			if err != nil {
				return nil, false, err
			}
			return key, true, nil
		}
	}
	return nil, false, nil
}

func (a *flatMapAccess) NextValueSeed(seed DeDeserializeSeed) (any, error) {
	value := a.pending
	a.pending = nil
	if value == nil {
		return nil, DeErrorCustom("value is missing")
	}
	return seed.DeserializeSeed(newContentRefDeserializer(value))
}

func (a *flatMapAccess) NextEntrySeed(keySeed DeDeserializeSeed, valueSeed DeDeserializeSeed) (any, any, bool, error) {
	return DeMapAccessDefaultNextEntrySeed(a, keySeed, valueSeed)
}

func (a *flatMapAccess) SizeHint() rs.Option[int] { return rs.None[int]() }

// flatStructAccess is `FlatStructAccess`: a flattened struct takes the
// entries whose keys are its fields.
type flatStructAccess struct {
	entries []flatMapEntry
	next    int
	pending *content
	fields  []string
}

func (a *flatStructAccess) NextKeySeed(seed DeDeserializeSeed) (any, bool, error) {
	for a.next < len(a.entries) {
		entry := flatMapTakeEntry(&a.entries[a.next], a.fields)
		a.next++
		if entry != nil {
			a.pending = &entry.value
			key, err := seed.DeserializeSeed(newContentDeserializer(&entry.key))
			if err != nil {
				return nil, false, err
			}
			return key, true, nil
		}
	}
	return nil, false, nil
}

func (a *flatStructAccess) NextValueSeed(seed DeDeserializeSeed) (any, error) {
	value := a.pending
	a.pending = nil
	if value == nil {
		return nil, DeErrorCustom("value is missing")
	}
	return seed.DeserializeSeed(newContentDeserializer(value))
}

func (a *flatStructAccess) NextEntrySeed(keySeed DeDeserializeSeed, valueSeed DeDeserializeSeed) (any, any, bool, error) {
	return DeMapAccessDefaultNextEntrySeed(a, keySeed, valueSeed)
}

func (a *flatStructAccess) SizeHint() rs.Option[int] { return rs.None[int]() }

// flatMapTakeEntry claims an entry of the FlatMapDeserializer buffer when
// it is recognized.
//
// Entries in the FlatMapDeserializer buffer are nulled out as they get
// claimed for deserialization. We only use an entry if it is still present
// and if the field is one recognized by the current data structure.
func flatMapTakeEntry(entry *flatMapEntry, recognized []string) *flatMapEntry {
	if !entry.present {
		return nil
	}
	name, ok := contentAsStr(&entry.key)
	if !ok || !slices.Contains(recognized, name) {
		return nil
	}
	entry.present = false
	return entry
}

// adjacentlyTaggedEnumVariantSeed is `AdjacentlyTaggedEnumVariantSeed<F>`:
// the tag of an adjacently tagged enum, read as an enum with unit variants.
// fieldSeed reads the F, which is the variant identifier, and keeps it.
type adjacentlyTaggedEnumVariantSeed struct {
	enumName  string
	variants  []string
	fieldSeed DeDeserializeSeed
}

func (s *adjacentlyTaggedEnumVariantSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeEnum(s.enumName, s.variants, s)
}

func (s *adjacentlyTaggedEnumVariantSeed) Expecting() string {
	return "variant of enum " + s.enumName
}

func (s *adjacentlyTaggedEnumVariantSeed) VisitEnum(data DeEnumAccess) (any, error) {
	variant, access, err := data.VariantSeed(s.fieldSeed)
	if err != nil {
		return nil, err
	}
	if err := access.UnitVariant(); err != nil {
		return nil, err
	}
	return variant, nil
}

// missingFieldDeserializer is the deserializer of `missing_field`: if the
// missing field is of type `Option<T>` then treat it as `None`, otherwise it
// is an error.
type missingFieldDeserializer struct{ field string }

func (d missingFieldDeserializer) missing() (any, error) {
	return nil, DeErrorMissingField(d.field)
}

func (d missingFieldDeserializer) DeserializeOption(visitor DeVisitor) (any, error) {
	return DeVisitorVisitNone(visitor)
}

func (d missingFieldDeserializer) DeserializeAny(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeBool(DeVisitor) (any, error)    { return d.missing() }
func (d missingFieldDeserializer) DeserializeI8(DeVisitor) (any, error)      { return d.missing() }
func (d missingFieldDeserializer) DeserializeI16(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeI32(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeI64(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeU8(DeVisitor) (any, error)      { return d.missing() }
func (d missingFieldDeserializer) DeserializeU16(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeU32(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeU64(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeF32(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeF64(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeChar(DeVisitor) (any, error)    { return d.missing() }
func (d missingFieldDeserializer) DeserializeStr(DeVisitor) (any, error)     { return d.missing() }
func (d missingFieldDeserializer) DeserializeString(DeVisitor) (any, error)  { return d.missing() }
func (d missingFieldDeserializer) DeserializeBytes(DeVisitor) (any, error)   { return d.missing() }
func (d missingFieldDeserializer) DeserializeByteBuf(DeVisitor) (any, error) { return d.missing() }
func (d missingFieldDeserializer) DeserializeUnit(DeVisitor) (any, error)    { return d.missing() }
func (d missingFieldDeserializer) DeserializeUnitStruct(string, DeVisitor) (any, error) {
	return d.missing()
}
func (d missingFieldDeserializer) DeserializeNewtypeStruct(string, DeVisitor) (any, error) {
	return d.missing()
}
func (d missingFieldDeserializer) DeserializeSeq(DeVisitor) (any, error)        { return d.missing() }
func (d missingFieldDeserializer) DeserializeTuple(int, DeVisitor) (any, error) { return d.missing() }
func (d missingFieldDeserializer) DeserializeTupleStruct(string, int, DeVisitor) (any, error) {
	return d.missing()
}
func (d missingFieldDeserializer) DeserializeMap(DeVisitor) (any, error) { return d.missing() }
func (d missingFieldDeserializer) DeserializeStruct(string, []string, DeVisitor) (any, error) {
	return d.missing()
}
func (d missingFieldDeserializer) DeserializeEnum(string, []string, DeVisitor) (any, error) {
	return d.missing()
}
func (d missingFieldDeserializer) DeserializeIdentifier(DeVisitor) (any, error) { return d.missing() }
func (d missingFieldDeserializer) DeserializeIgnoredAny(DeVisitor) (any, error) { return d.missing() }
func (d missingFieldDeserializer) IsHumanReadable() bool                        { return true }

var (
	_ Deserializer = (*contentDeserializer)(nil)
	_ Deserializer = (*contentRefDeserializer)(nil)
	_ Deserializer = flatMapDeserializer{}
	_ Deserializer = missingFieldDeserializer{}
)
