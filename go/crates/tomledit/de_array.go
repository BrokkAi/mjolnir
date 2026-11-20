// Port of toml_edit 0.20.2 src/de/array.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// arrayDeserializer is ArrayDeserializer.
type arrayDeserializer struct {
	deForward
	input []Item
	span  rs.Option[Range]
}

func arrayDeserializerNew(input []Item, span rs.Option[Range]) *arrayDeserializer {
	d := &arrayDeserializer{deForward: deForward{deserializeAny: nil}, input: input, span: span}
	d.deserializeAny = d.DeserializeAny
	return d
}

// Note: this is wrapped by `ValueDeserializer` and any trait methods
// implemented here need to be wrapped there

func (d *arrayDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitSeq(visitor, arraySeqAccessNew(d.input))
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *arrayDeserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
	if SpannedIsSpanned(name, fields) {
		if span, ok := d.span.Get(); ok {
			value, err := serde.DeVisitorVisitMap(visitor, spannedDeserializerNew(d, span))
			if err != nil {
				return nil, deErrorFrom(err)
			}
			return value, nil
		}
	}

	return d.DeserializeAny(visitor)
}

// arrayIntoDeserializer is `Array::into_deserializer`.
func arrayIntoDeserializer(a *Array) *arrayDeserializer {
	return arrayDeserializerNew(a.values, a.span)
}

// arrayOfTablesIntoDeserializer is `ArrayOfTables::into_deserializer`.
func arrayOfTablesIntoDeserializer(a *ArrayOfTables) *arrayDeserializer {
	return arrayDeserializerNew(a.values, a.span)
}

// arraySeqAccess is ArraySeqAccess.
type arraySeqAccess struct {
	iter []Item
}

func arraySeqAccessNew(input []Item) *arraySeqAccess {
	return &arraySeqAccess{iter: input}
}

func (s *arraySeqAccess) NextElementSeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if len(s.iter) == 0 {
		return nil, false, nil
	}
	v := s.iter[0]
	s.iter = s.iter[1:]
	value, err := seed.DeserializeSeed(deValueDeserializerNew(v))
	if err != nil {
		return nil, false, deErrorFrom(err)
	}
	return value, true, nil
}

func (s *arraySeqAccess) SizeHint() rs.Option[int] { return noSizeHint() }
