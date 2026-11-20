// Port of toml_edit 0.20.2 src/de/table.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// tableDeserializer is TableDeserializer.
type tableDeserializer struct {
	deForward
	span  rs.Option[Range]
	items keyValuePairs
}

func tableDeserializerNew(span rs.Option[Range], items keyValuePairs) *tableDeserializer {
	d := &tableDeserializer{deForward: deForward{deserializeAny: nil}, span: span, items: items}
	d.deserializeAny = d.DeserializeAny
	return d
}

// Note: this is wrapped by `Deserializer` and `ValueDeserializer` and any trait methods
// implemented here need to be wrapped there

func (d *tableDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitMap(visitor, tableMapAccessNew(d))
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

// `None` is interpreted as a missing field so be sure to implement `Some`
// as a present field.
func (d *tableDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitSome(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *tableDeserializer) DeserializeNewtypeStruct(_ string, visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitNewtypeStruct(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *tableDeserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
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

// Called when the type to deserialize is an enum, as opposed to a field in the type.
func (d *tableDeserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	if d.items.len() == 0 {
		return nil, deErrorCustom("wanted exactly 1 element, found 0 elements", d.span)
	} else if d.items.len() != 1 {
		return nil, deErrorCustom("wanted exactly 1 element, more than 1 element", d.span)
	}
	value, err := serde.DeVisitorVisitEnum(visitor, tableMapAccessNew(d))
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

// tableIntoDeserializer is `Table::into_deserializer`.
func tableIntoDeserializer(t *Table) *tableDeserializer {
	return tableDeserializerNew(t.span, t.items)
}

// inlineTableIntoDeserializer is `InlineTable::into_deserializer`.
func inlineTableIntoDeserializer(t *InlineTable) *tableDeserializer {
	return tableDeserializerNew(t.span, t.items)
}

// tableMapAccess is TableMapAccess.
type tableMapAccess struct {
	iter  []*tableKeyValue
	span  rs.Option[Range]
	value rs.Option[rs.Tuple2[InternalString, Item]]
}

func tableMapAccessNew(input *tableDeserializer) *tableMapAccess {
	return &tableMapAccess{
		iter:  input.items.snapshot(),
		span:  input.span,
		value: rs.None[rs.Tuple2[InternalString, Item]](),
	}
}

// next is the IndexMap iterator's next: the map key and the pair.
func (m *tableMapAccess) next() (*tableKeyValue, bool) {
	if len(m.iter) == 0 {
		return nil, false
	}
	kv := m.iter[0]
	m.iter = m.iter[1:]
	return kv, true
}

func (m *tableMapAccess) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	v, ok := m.next()
	if !ok {
		return nil, false, nil
	}
	key, err := seed.DeserializeSeed(keyDeserializerNew(v.mapKey, v.key.span()))
	if err != nil {
		err = spanIfNone(err, v.key.span())
	}
	m.value = rs.Some(rs.Tuple2[InternalString, Item]{F0: v.key.Get(), F1: v.value})
	if err != nil {
		return nil, false, err
	}
	return key, true, nil
}

func (m *tableMapAccess) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	pair, ok := m.value.Take().Get()
	if !ok {
		panic(rs.Panicf("no more values in next_value_seed, internal error in ValueDeserializer"))
	}
	k, v := pair.F0, pair.F1
	span := v.span()
	value, err := seed.DeserializeSeed(deValueDeserializerNew(v))
	if err != nil {
		e := deErrorFrom(err)
		if e.Span().IsNone() {
			e.setSpan(span)
		}
		e.AddKey(k)
		return nil, e
	}
	return value, nil
}

func (m *tableMapAccess) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(m, keySeed, valueSeed)
}

func (m *tableMapAccess) SizeHint() rs.Option[int] { return noSizeHint() }

// VariantSeed is `impl EnumAccess for TableMapAccess`.
func (m *tableMapAccess) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	value, ok := m.next()
	if !ok {
		return nil, nil, deErrorCustom("expected table with exactly 1 entry, found empty table", m.span)
	}
	key := value.mapKey

	val, err := seed.DeserializeSeed(strDeserializerNew(key))
	if err != nil {
		return nil, nil, spanIfNone(err, value.key.span())
	}

	variant := tableEnumDeserializerNew(value.value)

	return val, variant, nil
}
