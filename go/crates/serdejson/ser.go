// Port of serde_json 1.0.150 src/ser.rs

package serdejson

import (
	"bytes"
	"io"
	"math"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Serialize a Rust data structure into JSON data.

// Serializer is `serde_json::Serializer`: a structure for serializing Rust
// values into JSON. It implements serde.Serializer; its `Ok` is nil.
type Serializer struct {
	writer    io.Writer
	formatter Formatter
}

// SerializerNew is `Serializer::new`: a new JSON serializer.
func SerializerNew(writer io.Writer) *Serializer {
	return SerializerWithFormatter(writer, &CompactFormatter{})
}

// SerializerPretty is `Serializer::pretty`: a new JSON pretty print
// serializer.
func SerializerPretty(writer io.Writer) *Serializer {
	return SerializerWithFormatter(writer, PrettyFormatterNew())
}

// SerializerWithFormatter is `Serializer::with_formatter`: a new JSON visitor
// whose output will be written to the writer specified.
func SerializerWithFormatter(writer io.Writer, formatter Formatter) *Serializer {
	return &Serializer{writer: writer, formatter: formatter}
}

// IntoInner is `Serializer::into_inner`: unwrap the `Writer` from the
// `Serializer`.
func (s *Serializer) IntoInner() io.Writer { return s.writer }

func ioResult(err error) (any, error) {
	if err != nil {
		return nil, errorIo(err)
	}
	return nil, nil
}

func ioErr(err error) error {
	if err != nil {
		return errorIo(err)
	}
	return nil
}

// serialize hands value to serde with s as the serializer, taking an
// error that comes back as this format's.
func (s *Serializer) serialize(value any) error {
	_, err := serde.Serialize(value, s)
	if err != nil {
		return toError(err)
	}
	return nil
}

func (s *Serializer) SerializeBool(value bool) (any, error) {
	return ioResult(s.formatter.WriteBool(s.writer, value))
}

func (s *Serializer) SerializeI8(value int8) (any, error) {
	return ioResult(s.formatter.WriteI64(s.writer, int64(value)))
}

func (s *Serializer) SerializeI16(value int16) (any, error) {
	return ioResult(s.formatter.WriteI64(s.writer, int64(value)))
}

func (s *Serializer) SerializeI32(value int32) (any, error) {
	return ioResult(s.formatter.WriteI64(s.writer, int64(value)))
}

func (s *Serializer) SerializeI64(value int64) (any, error) {
	return ioResult(s.formatter.WriteI64(s.writer, value))
}

func (s *Serializer) SerializeU8(value uint8) (any, error) {
	return ioResult(s.formatter.WriteU64(s.writer, uint64(value)))
}

func (s *Serializer) SerializeU16(value uint16) (any, error) {
	return ioResult(s.formatter.WriteU64(s.writer, uint64(value)))
}

func (s *Serializer) SerializeU32(value uint32) (any, error) {
	return ioResult(s.formatter.WriteU64(s.writer, uint64(value)))
}

func (s *Serializer) SerializeU64(value uint64) (any, error) {
	return ioResult(s.formatter.WriteU64(s.writer, value))
}

// SerializeI128 is `serialize_i128`.
func (s *Serializer) SerializeI128(value rs.I128) (any, error) {
	return ioResult(s.formatter.WriteNumberStr(s.writer, value.String()))
}

// SerializeU128 is `serialize_u128`.
func (s *Serializer) SerializeU128(value rs.U128) (any, error) {
	return ioResult(s.formatter.WriteNumberStr(s.writer, value.String()))
}

func (s *Serializer) SerializeF32(value float32) (any, error) {
	if math.IsNaN(float64(value)) || math.IsInf(float64(value), 0) {
		return ioResult(s.formatter.WriteNull(s.writer))
	}
	return ioResult(s.formatter.WriteF32(s.writer, value))
}

func (s *Serializer) SerializeF64(value float64) (any, error) {
	if math.IsNaN(value) || math.IsInf(value, 0) {
		return ioResult(s.formatter.WriteNull(s.writer))
	}
	return ioResult(s.formatter.WriteF64(s.writer, value))
}

func (s *Serializer) SerializeChar(value rune) (any, error) {
	// A char encoded as UTF-8 takes 4 bytes at most.
	return s.SerializeStr(string(value))
}

func (s *Serializer) SerializeStr(value string) (any, error) {
	return ioResult(formatEscapedStr(s.writer, s.formatter, value))
}

func (s *Serializer) SerializeBytes(value []byte) (any, error) {
	return ioResult(s.formatter.WriteByteArray(s.writer, value))
}

func (s *Serializer) SerializeUnit() (any, error) {
	return ioResult(s.formatter.WriteNull(s.writer))
}

func (s *Serializer) SerializeUnitStruct(string) (any, error) { return s.SerializeUnit() }

func (s *Serializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return s.SerializeStr(variant)
}

// SerializeNewtypeStruct serializes newtypes without an object wrapper.
func (s *Serializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return nil, s.serialize(value)
}

// beginVariant writes `{"variant":` for the variants written as an object
// with one entry.
func (s *Serializer) beginVariant(variant string) error {
	if err := s.formatter.BeginObject(s.writer); err != nil {
		return errorIo(err)
	}
	if err := s.formatter.BeginObjectKey(s.writer, true); err != nil {
		return errorIo(err)
	}
	if _, err := s.SerializeStr(variant); err != nil {
		return err
	}
	if err := s.formatter.EndObjectKey(s.writer); err != nil {
		return errorIo(err)
	}
	return ioErr(s.formatter.BeginObjectValue(s.writer))
}

func (s *Serializer) SerializeNewtypeVariant(_ string, _ uint32, variant string, value any) (any, error) {
	if err := s.beginVariant(variant); err != nil {
		return nil, err
	}
	if err := s.serialize(value); err != nil {
		return nil, err
	}
	if err := s.formatter.EndObjectValue(s.writer); err != nil {
		return nil, errorIo(err)
	}
	return ioResult(s.formatter.EndObject(s.writer))
}

func (s *Serializer) SerializeNone() (any, error) { return s.SerializeUnit() }

func (s *Serializer) SerializeSome(value any) (any, error) { return nil, s.serialize(value) }

func (s *Serializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	if err := s.formatter.BeginArray(s.writer); err != nil {
		return nil, errorIo(err)
	}
	if n, ok := length.Get(); ok && n == 0 {
		if err := s.formatter.EndArray(s.writer); err != nil {
			return nil, errorIo(err)
		}
		return &compound{ser: s, state: stateEmpty}, nil
	}
	return &compound{ser: s, state: stateFirst}, nil
}

func (s *Serializer) SerializeTuple(length int) (serde.SerSerializeTuple, error) {
	return s.SerializeSeq(rs.Some(length))
}

func (s *Serializer) SerializeTupleStruct(_ string, length int) (serde.SerSerializeTupleStruct, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*compound), nil
}

func (s *Serializer) SerializeTupleVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeTupleVariant, error) {
	if err := s.beginVariant(variant); err != nil {
		return nil, err
	}
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return &tupleVariantCompound{seq.(*compound)}, nil
}

func (s *Serializer) SerializeMap(length rs.Option[int]) (serde.SerSerializeMap, error) {
	if err := s.formatter.BeginObject(s.writer); err != nil {
		return nil, errorIo(err)
	}
	if n, ok := length.Get(); ok && n == 0 {
		if err := s.formatter.EndObject(s.writer); err != nil {
			return nil, errorIo(err)
		}
		return &compound{ser: s, state: stateEmpty, isMap: true}, nil
	}
	return &compound{ser: s, state: stateFirst, isMap: true}, nil
}

func (s *Serializer) SerializeStruct(name string, length int) (serde.SerSerializeStruct, error) {
	if name == rawValueToken {
		return &rawValueCompound{ser: s}, nil
	}
	m, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return &structCompound{m.(*compound)}, nil
}

func (s *Serializer) SerializeStructVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeStructVariant, error) {
	if err := s.beginVariant(variant); err != nil {
		return nil, err
	}
	m, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return &structVariantCompound{m.(*compound)}, nil
}

func (s *Serializer) CollectStr(value string) (any, error) {
	if err := s.formatter.BeginString(s.writer); err != nil {
		return nil, errorIo(err)
	}
	if err := formatEscapedStrContents(s.writer, s.formatter, value); err != nil {
		return nil, errorIo(err)
	}
	return ioResult(s.formatter.EndString(s.writer))
}

func (s *Serializer) IsHumanReadable() bool { return true }

// state is `State`.
type state uint8

const (
	stateEmpty state = iota
	stateFirst
	stateRest
)

// compound is `Compound::Map`: what the Serialize* traits are written
// through.
type compound struct {
	ser   *Serializer
	state state
	// isMap says the compound writes an object rather than an array.
	isMap bool
}

func (c *compound) SerializeElement(value any) error {
	if err := c.ser.formatter.BeginArrayValue(c.ser.writer, c.state == stateFirst); err != nil {
		return errorIo(err)
	}
	c.state = stateRest
	if err := c.ser.serialize(value); err != nil {
		return err
	}
	return ioErr(c.ser.formatter.EndArrayValue(c.ser.writer))
}

func (c *compound) endSeq() (any, error) {
	if c.state == stateEmpty {
		return nil, nil
	}
	return ioResult(c.ser.formatter.EndArray(c.ser.writer))
}

func (c *compound) SerializeKey(key any) error {
	if err := c.ser.formatter.BeginObjectKey(c.ser.writer, c.state == stateFirst); err != nil {
		return errorIo(err)
	}
	c.state = stateRest
	if _, err := serde.Serialize(key, &mapKeySerializer{ser: c.ser}); err != nil {
		return toError(err)
	}
	return ioErr(c.ser.formatter.EndObjectKey(c.ser.writer))
}

func (c *compound) SerializeValue(value any) error {
	if err := c.ser.formatter.BeginObjectValue(c.ser.writer); err != nil {
		return errorIo(err)
	}
	if err := c.ser.serialize(value); err != nil {
		return err
	}
	return ioErr(c.ser.formatter.EndObjectValue(c.ser.writer))
}

func (c *compound) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(c, key, value)
}

func (c *compound) endMap() (any, error) {
	if c.state == stateEmpty {
		return nil, nil
	}
	return ioResult(c.ser.formatter.EndObject(c.ser.writer))
}

// End finishes a seq, tuple or tuple struct, and a map or struct.
// compound is returned for both, so it tells them apart by what it wrote.
func (c *compound) End() (any, error) {
	if c.isMap {
		return c.endMap()
	}
	return c.endSeq()
}

func (c *compound) SerializeField(value any) error { return c.SerializeElement(value) }

// structCompound is a Compound used as `SerializeStruct`.
type structCompound struct{ *compound }

func (c *structCompound) SerializeField(key string, value any) error {
	return c.SerializeEntry(key, value)
}

func (c *structCompound) SkipField(string) error { return nil }

// tupleVariantCompound is a Compound used as `SerializeTupleVariant`.
type tupleVariantCompound struct{ *compound }

func (c *tupleVariantCompound) SerializeField(value any) error { return c.SerializeElement(value) }

func (c *tupleVariantCompound) End() (any, error) {
	if c.state != stateEmpty {
		if err := c.ser.formatter.EndArray(c.ser.writer); err != nil {
			return nil, errorIo(err)
		}
	}
	if err := c.ser.formatter.EndObjectValue(c.ser.writer); err != nil {
		return nil, errorIo(err)
	}
	return ioResult(c.ser.formatter.EndObject(c.ser.writer))
}

// structVariantCompound is a Compound used as `SerializeStructVariant`.
type structVariantCompound struct{ *compound }

func (c *structVariantCompound) SerializeField(key string, value any) error {
	return c.SerializeEntry(key, value)
}

func (c *structVariantCompound) SkipField(string) error { return nil }

func (c *structVariantCompound) End() (any, error) {
	if c.state != stateEmpty {
		if err := c.ser.formatter.EndObject(c.ser.writer); err != nil {
			return nil, errorIo(err)
		}
	}
	if err := c.ser.formatter.EndObjectValue(c.ser.writer); err != nil {
		return nil, errorIo(err)
	}
	return ioResult(c.ser.formatter.EndObject(c.ser.writer))
}

// rawValueCompound is `Compound::RawValue`.
type rawValueCompound struct{ ser *Serializer }

func (c *rawValueCompound) SerializeField(key string, value any) error {
	if key != rawValueToken {
		return errorSyntax(errorCodeExpectedSomeValue, 0, 0)
	}
	if _, err := serde.Serialize(value, &rawValueStrEmitter{ser: c.ser}); err != nil {
		return toError(err)
	}
	return nil
}

func (c *rawValueCompound) SkipField(string) error { return nil }
func (c *rawValueCompound) End() (any, error)      { return nil, nil }

func keyMustBeAString() error { return errorSyntax(errorCodeKeyMustBeAString, 0, 0) }

func floatKeyMustBeFinite() error { return errorSyntax(errorCodeFloatKeyMustBeFinite, 0, 0) }

// mapKeySerializer is `MapKeySerializer`: what a map key is serialized
// with. A key must come out as a JSON string.
type mapKeySerializer struct{ ser *Serializer }

func (k *mapKeySerializer) quoted(write func() error) (any, error) {
	if err := k.ser.formatter.BeginString(k.ser.writer); err != nil {
		return nil, errorIo(err)
	}
	if err := write(); err != nil {
		return nil, errorIo(err)
	}
	return ioResult(k.ser.formatter.EndString(k.ser.writer))
}

func (k *mapKeySerializer) SerializeStr(value string) (any, error) {
	return k.ser.SerializeStr(value)
}

func (k *mapKeySerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return k.ser.SerializeStr(variant)
}

func (k *mapKeySerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	_, err := serde.Serialize(value, k)
	return nil, err
}

func (k *mapKeySerializer) SerializeBool(value bool) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteBool(k.ser.writer, value) })
}

func (k *mapKeySerializer) SerializeI8(value int8) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteI64(k.ser.writer, int64(value)) })
}

func (k *mapKeySerializer) SerializeI16(value int16) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteI64(k.ser.writer, int64(value)) })
}

func (k *mapKeySerializer) SerializeI32(value int32) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteI64(k.ser.writer, int64(value)) })
}

func (k *mapKeySerializer) SerializeI64(value int64) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteI64(k.ser.writer, value) })
}

func (k *mapKeySerializer) SerializeU8(value uint8) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteU64(k.ser.writer, uint64(value)) })
}

func (k *mapKeySerializer) SerializeU16(value uint16) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteU64(k.ser.writer, uint64(value)) })
}

func (k *mapKeySerializer) SerializeU32(value uint32) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteU64(k.ser.writer, uint64(value)) })
}

func (k *mapKeySerializer) SerializeU64(value uint64) (any, error) {
	return k.quoted(func() error { return k.ser.formatter.WriteU64(k.ser.writer, value) })
}

func (k *mapKeySerializer) SerializeF32(value float32) (any, error) {
	if math.IsNaN(float64(value)) || math.IsInf(float64(value), 0) {
		return nil, floatKeyMustBeFinite()
	}
	return k.quoted(func() error { return k.ser.formatter.WriteF32(k.ser.writer, value) })
}

func (k *mapKeySerializer) SerializeF64(value float64) (any, error) {
	if math.IsNaN(value) || math.IsInf(value, 0) {
		return nil, floatKeyMustBeFinite()
	}
	return k.quoted(func() error { return k.ser.formatter.WriteF64(k.ser.writer, value) })
}

func (k *mapKeySerializer) SerializeChar(value rune) (any, error) {
	return k.ser.SerializeStr(string(value))
}

func (k *mapKeySerializer) SerializeBytes([]byte) (any, error) { return nil, keyMustBeAString() }
func (k *mapKeySerializer) SerializeUnit() (any, error)        { return nil, keyMustBeAString() }
func (k *mapKeySerializer) SerializeUnitStruct(string) (any, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeNewtypeVariant(string, uint32, string, any) (any, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeNone() (any, error) { return nil, keyMustBeAString() }

func (k *mapKeySerializer) SerializeSome(value any) (any, error) {
	_, err := serde.Serialize(value, k)
	return nil, err
}

func (k *mapKeySerializer) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeTupleStruct(string, int) (serde.SerSerializeTupleStruct, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeTupleVariant(string, uint32, string, int) (serde.SerSerializeTupleVariant, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeStruct(string, int) (serde.SerSerializeStruct, error) {
	return nil, keyMustBeAString()
}
func (k *mapKeySerializer) SerializeStructVariant(string, uint32, string, int) (serde.SerSerializeStructVariant, error) {
	return nil, keyMustBeAString()
}

func (k *mapKeySerializer) CollectStr(value string) (any, error) { return k.ser.CollectStr(value) }

func (k *mapKeySerializer) IsHumanReadable() bool { return true }

// rawValueStrEmitter is `RawValueStrEmitter`: it writes the text of a
// RawValue as it is.
type rawValueStrEmitter struct{ ser *Serializer }

func expectedRawValue() error { return serde.SerErrorCustom("expected RawValue") }

func (e *rawValueStrEmitter) SerializeStr(value string) (any, error) {
	return ioResult(e.ser.formatter.WriteRawFragment(e.ser.writer, value))
}

func (e *rawValueStrEmitter) SerializeBool(bool) (any, error)         { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeI8(int8) (any, error)           { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeI16(int16) (any, error)         { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeI32(int32) (any, error)         { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeI64(int64) (any, error)         { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeU8(uint8) (any, error)          { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeU16(uint16) (any, error)        { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeU32(uint32) (any, error)        { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeU64(uint64) (any, error)        { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeF32(float32) (any, error)       { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeF64(float64) (any, error)       { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeChar(rune) (any, error)         { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeBytes([]byte) (any, error)      { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeNone() (any, error)             { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeSome(any) (any, error)          { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeUnit() (any, error)             { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeUnitStruct(string) (any, error) { return nil, expectedRawValue() }
func (e *rawValueStrEmitter) SerializeUnitVariant(string, uint32, string) (any, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeNewtypeStruct(string, any) (any, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeNewtypeVariant(string, uint32, string, any) (any, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeTupleStruct(string, int) (serde.SerSerializeTupleStruct, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeTupleVariant(string, uint32, string, int) (serde.SerSerializeTupleVariant, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeStruct(string, int) (serde.SerSerializeStruct, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) SerializeStructVariant(string, uint32, string, int) (serde.SerSerializeStructVariant, error) {
	return nil, expectedRawValue()
}
func (e *rawValueStrEmitter) CollectStr(value string) (any, error) { return e.SerializeStr(value) }
func (e *rawValueStrEmitter) IsHumanReadable() bool                { return true }

// CharEscape is `serde_json::ser::CharEscape`: the kind of escape a
// character gets.
type CharEscape struct {
	// Kind is the escape: one of '"', '\\', '/', 'b', 'f', 'n', 'r', 't',
	// or 'u' for an ASCII control character written as \u00XX.
	Kind byte
	// Byte is the control character of a 'u' escape.
	Byte byte
}

// Formatter is `serde_json::ser::Formatter`: how a Serializer writes JSON.
// Embed CompactFormatter for the trait's default methods.
type Formatter interface {
	WriteNull(w io.Writer) error
	WriteBool(w io.Writer, value bool) error
	// WriteI64 writes any signed integer (write_i8 .. write_i64).
	WriteI64(w io.Writer, value int64) error
	// WriteU64 writes any unsigned integer (write_u8 .. write_u64).
	WriteU64(w io.Writer, value uint64) error
	WriteF32(w io.Writer, value float32) error
	WriteF64(w io.Writer, value float64) error
	WriteNumberStr(w io.Writer, value string) error
	BeginString(w io.Writer) error
	EndString(w io.Writer) error
	WriteStringFragment(w io.Writer, fragment string) error
	WriteCharEscape(w io.Writer, escape CharEscape) error
	WriteByteArray(w io.Writer, value []byte) error
	BeginArray(w io.Writer) error
	EndArray(w io.Writer) error
	BeginArrayValue(w io.Writer, first bool) error
	EndArrayValue(w io.Writer) error
	BeginObject(w io.Writer) error
	EndObject(w io.Writer) error
	BeginObjectKey(w io.Writer, first bool) error
	EndObjectKey(w io.Writer) error
	BeginObjectValue(w io.Writer) error
	EndObjectValue(w io.Writer) error
	WriteRawFragment(w io.Writer, fragment string) error
}

func writeAll(w io.Writer, text string) error {
	_, err := io.WriteString(w, text)
	return err
}

// CompactFormatter is `serde_json::ser::CompactFormatter`: it does not add
// any whitespace to the output. Its methods are the trait's defaults.
type CompactFormatter struct {
	// self is the formatter whose methods WriteByteArray calls; a formatter
	// that embeds CompactFormatter and overrides the array methods sets it
	// to itself.
	self Formatter
}

func (f *CompactFormatter) outer() Formatter {
	if f.self != nil {
		return f.self
	}
	return f
}

func (f *CompactFormatter) WriteNull(w io.Writer) error { return writeAll(w, "null") }

func (f *CompactFormatter) WriteBool(w io.Writer, value bool) error {
	if value {
		return writeAll(w, "true")
	}
	return writeAll(w, "false")
}

func (f *CompactFormatter) WriteI64(w io.Writer, value int64) error {
	return writeAll(w, strconv.FormatInt(value, 10))
}

func (f *CompactFormatter) WriteU64(w io.Writer, value uint64) error {
	return writeAll(w, strconv.FormatUint(value, 10))
}

func (f *CompactFormatter) WriteF32(w io.Writer, value float32) error {
	return writeAll(w, formatF32(value))
}

func (f *CompactFormatter) WriteF64(w io.Writer, value float64) error {
	return writeAll(w, formatF64(value))
}

func (f *CompactFormatter) WriteNumberStr(w io.Writer, value string) error {
	return writeAll(w, value)
}

func (f *CompactFormatter) BeginString(w io.Writer) error { return writeAll(w, `"`) }
func (f *CompactFormatter) EndString(w io.Writer) error   { return writeAll(w, `"`) }

func (f *CompactFormatter) WriteStringFragment(w io.Writer, fragment string) error {
	return writeAll(w, fragment)
}

const hexDigits = "0123456789abcdef"

func (f *CompactFormatter) WriteCharEscape(w io.Writer, escape CharEscape) error {
	if escape.Kind == 'u' {
		return writeAll(w, string([]byte{'\\', 'u', '0', '0', hexDigits[escape.Byte>>4], hexDigits[escape.Byte&0xF]}))
	}
	return writeAll(w, string([]byte{'\\', escape.Kind}))
}

func (f *CompactFormatter) WriteByteArray(w io.Writer, value []byte) error {
	self := f.outer()
	if err := self.BeginArray(w); err != nil {
		return err
	}
	first := true
	for _, b := range value {
		if err := self.BeginArrayValue(w, first); err != nil {
			return err
		}
		if err := self.WriteU64(w, uint64(b)); err != nil {
			return err
		}
		if err := self.EndArrayValue(w); err != nil {
			return err
		}
		first = false
	}
	return self.EndArray(w)
}

func (f *CompactFormatter) BeginArray(w io.Writer) error { return writeAll(w, "[") }
func (f *CompactFormatter) EndArray(w io.Writer) error   { return writeAll(w, "]") }

func (f *CompactFormatter) BeginArrayValue(w io.Writer, first bool) error {
	if first {
		return nil
	}
	return writeAll(w, ",")
}

func (f *CompactFormatter) EndArrayValue(io.Writer) error { return nil }
func (f *CompactFormatter) BeginObject(w io.Writer) error { return writeAll(w, "{") }
func (f *CompactFormatter) EndObject(w io.Writer) error   { return writeAll(w, "}") }

func (f *CompactFormatter) BeginObjectKey(w io.Writer, first bool) error {
	if first {
		return nil
	}
	return writeAll(w, ",")
}

func (f *CompactFormatter) EndObjectKey(io.Writer) error       { return nil }
func (f *CompactFormatter) BeginObjectValue(w io.Writer) error { return writeAll(w, ":") }
func (f *CompactFormatter) EndObjectValue(io.Writer) error     { return nil }

func (f *CompactFormatter) WriteRawFragment(w io.Writer, fragment string) error {
	return writeAll(w, fragment)
}

// PrettyFormatter is `serde_json::ser::PrettyFormatter`: it adds
// whitespace to make the JSON easier for humans to read.
type PrettyFormatter struct {
	CompactFormatter
	currentIndent int
	hasValue      bool
	indent        string
}

// PrettyFormatterNew is `PrettyFormatter::new`: a pretty printer formatter
// that defaults to an indentation of two spaces.
func PrettyFormatterNew() *PrettyFormatter { return PrettyFormatterWithIndent("  ") }

// PrettyFormatterWithIndent is `PrettyFormatter::with_indent`: a pretty
// printer formatter that uses the indent string for indentation.
func PrettyFormatterWithIndent(indent string) *PrettyFormatter {
	f := &PrettyFormatter{indent: indent}
	f.CompactFormatter.self = f
	return f
}

func (f *PrettyFormatter) writeIndent(w io.Writer) error {
	for range f.currentIndent {
		if err := writeAll(w, f.indent); err != nil {
			return err
		}
	}
	return nil
}

func (f *PrettyFormatter) BeginArray(w io.Writer) error {
	f.currentIndent++
	f.hasValue = false
	return writeAll(w, "[")
}

func (f *PrettyFormatter) EndArray(w io.Writer) error {
	f.currentIndent--
	if f.hasValue {
		if err := writeAll(w, "\n"); err != nil {
			return err
		}
		if err := f.writeIndent(w); err != nil {
			return err
		}
	}
	return writeAll(w, "]")
}

func (f *PrettyFormatter) BeginArrayValue(w io.Writer, first bool) error {
	separator := ",\n"
	if first {
		separator = "\n"
	}
	if err := writeAll(w, separator); err != nil {
		return err
	}
	return f.writeIndent(w)
}

func (f *PrettyFormatter) EndArrayValue(io.Writer) error {
	f.hasValue = true
	return nil
}

func (f *PrettyFormatter) BeginObject(w io.Writer) error {
	f.currentIndent++
	f.hasValue = false
	return writeAll(w, "{")
}

func (f *PrettyFormatter) EndObject(w io.Writer) error {
	f.currentIndent--
	if f.hasValue {
		if err := writeAll(w, "\n"); err != nil {
			return err
		}
		if err := f.writeIndent(w); err != nil {
			return err
		}
	}
	return writeAll(w, "}")
}

func (f *PrettyFormatter) BeginObjectKey(w io.Writer, first bool) error {
	separator := ",\n"
	if first {
		separator = "\n"
	}
	if err := writeAll(w, separator); err != nil {
		return err
	}
	return f.writeIndent(w)
}

func (f *PrettyFormatter) BeginObjectValue(w io.Writer) error { return writeAll(w, ": ") }

func (f *PrettyFormatter) EndObjectValue(io.Writer) error {
	f.hasValue = true
	return nil
}

// formatEscapedStr is `format_escaped_str`.
func formatEscapedStr(w io.Writer, formatter Formatter, value string) error {
	if err := formatter.BeginString(w); err != nil {
		return err
	}
	if err := formatEscapedStrContents(w, formatter, value); err != nil {
		return err
	}
	return formatter.EndString(w)
}

// formatEscapedStrContents is `format_escaped_str_contents`.
func formatEscapedStrContents(w io.Writer, formatter Formatter, value string) error {
	start := 0
	for i := 0; i < len(value); i++ {
		escape := escapeTable[value[i]]
		if escape == 0 {
			continue
		}
		if start < i {
			if err := formatter.WriteStringFragment(w, value[start:i]); err != nil {
				return err
			}
		}
		charEscape := CharEscape{Kind: escape}
		if escape == 'u' {
			charEscape.Byte = value[i]
		}
		if err := formatter.WriteCharEscape(w, charEscape); err != nil {
			return err
		}
		start = i + 1
	}
	if start == len(value) {
		return nil
	}
	return formatter.WriteStringFragment(w, value[start:])
}

// escapeTable is `ESCAPE`: a value of 'x' at index i means that byte i is
// escaped as "\x" in JSON. A value of 0 means that byte i is not escaped.
var escapeTable = func() [256]byte {
	var table [256]byte
	for i := range 0x20 {
		table[i] = 'u'
	}
	table['\b'] = 'b'
	table['\t'] = 't'
	table['\n'] = 'n'
	table['\f'] = 'f'
	table['\r'] = 'r'
	table['"'] = '"'
	table['\\'] = '\\'
	return table
}()

// ToWriter is `serde_json::to_writer`: serialize the given data structure
// as JSON into the I/O stream.
func ToWriter(writer io.Writer, value any) error {
	return SerializerNew(writer).serialize(value)
}

// ToWriterPretty is `serde_json::to_writer_pretty`: serialize the given
// data structure as pretty-printed JSON into the I/O stream.
func ToWriterPretty(writer io.Writer, value any) error {
	return SerializerPretty(writer).serialize(value)
}

// ToVec is `serde_json::to_vec`: serialize the given data structure as a
// JSON byte vector.
func ToVec(value any) ([]byte, error) {
	var buffer bytes.Buffer
	buffer.Grow(128)
	if err := ToWriter(&buffer, value); err != nil {
		return nil, err
	}
	return buffer.Bytes(), nil
}

// ToVecPretty is `serde_json::to_vec_pretty`: serialize the given data
// structure as a pretty-printed JSON byte vector.
func ToVecPretty(value any) ([]byte, error) {
	var buffer bytes.Buffer
	buffer.Grow(128)
	if err := ToWriterPretty(&buffer, value); err != nil {
		return nil, err
	}
	return buffer.Bytes(), nil
}

// ToString is `serde_json::to_string`: serialize the given data structure
// as a String of JSON.
func ToString(value any) (string, error) {
	bytes, err := ToVec(value)
	if err != nil {
		return "", err
	}
	return string(bytes), nil
}

// ToStringPretty is `serde_json::to_string_pretty`: serialize the given
// data structure as a pretty-printed String of JSON.
func ToStringPretty(value any) (string, error) {
	bytes, err := ToVecPretty(value)
	if err != nil {
		return "", err
	}
	return string(bytes), nil
}

var _ serde.Serializer = (*Serializer)(nil)
var _ serde.Serializer = (*mapKeySerializer)(nil)
var _ serde.Serializer = (*rawValueStrEmitter)(nil)
