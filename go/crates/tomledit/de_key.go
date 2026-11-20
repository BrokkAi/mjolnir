// Port of toml_edit 0.20.2 src/de/key.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// keyDeserializer is KeyDeserializer: what a table's key is read with.
type keyDeserializer struct {
	deForward
	span rs.Option[Range]
	key  InternalString
}

func keyDeserializerNew(key InternalString, span rs.Option[Range]) *keyDeserializer {
	d := &keyDeserializer{deForward: deForward{deserializeAny: nil}, span: span, key: key}
	d.deserializeAny = d.DeserializeAny
	return d
}

func (d *keyDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	value, err := strDeserializerNew(d.key).DeserializeAny(visitor)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *keyDeserializer) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	_ = name
	_ = variants
	value, err := serde.DeVisitorVisitEnum(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *keyDeserializer) DeserializeStruct(name string, fields []string, visitor serde.DeVisitor) (any, error) {
	if SpannedIsSpanned(name, fields) {
		if span, ok := d.span.Get(); ok {
			value, err := serde.DeVisitorVisitMap(visitor, spannedDeserializerNew(strDeserializerNew(d.key), span))
			if err != nil {
				return nil, deErrorFrom(err)
			}
			return value, nil
		}
	}
	return d.DeserializeAny(visitor)
}

func (d *keyDeserializer) DeserializeNewtypeStruct(_ string, visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitNewtypeStruct(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

// VariantSeed is `impl EnumAccess for KeyDeserializer`.
func (d *keyDeserializer) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	value, err := seed.DeserializeSeed(d)
	if err != nil {
		return nil, nil, deErrorFrom(err)
	}
	return value, unitOnly{}, nil
}
