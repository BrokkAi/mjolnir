// Port of toml_edit 0.20.2 src/de/spanned.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// spannedDeserializer is SpannedDeserializer: the map a `Spanned<T>` reads
// its start, end and value from.
//
// Rust keeps the value as a `T: IntoDeserializer` and converts it when it is
// read; every T toml_edit uses (its own deserializers, and `&str`, which is a
// StrDeserializer) converts to a deserializer without doing anything else, so
// the value is held as that deserializer.
type spannedDeserializer struct {
	start rs.Option[int]
	end   rs.Option[int]
	value rs.Option[serde.Deserializer]
}

func spannedDeserializerNew(value serde.Deserializer, span Range) *spannedDeserializer {
	return &spannedDeserializer{
		start: rs.Some(span.Start),
		end:   rs.Some(span.End),
		value: rs.Some(value),
	}
}

func (d *spannedDeserializer) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	var key any
	var err error
	switch {
	case d.start.IsSome():
		key, err = seed.DeserializeSeed(borrowedStrDeserializerNew(SpannedStartField))
	case d.end.IsSome():
		key, err = seed.DeserializeSeed(borrowedStrDeserializerNew(SpannedEndField))
	case d.value.IsSome():
		key, err = seed.DeserializeSeed(borrowedStrDeserializerNew(SpannedValueField))
	default:
		return nil, false, nil
	}
	if err != nil {
		return nil, false, deErrorFrom(err)
	}
	return key, true, nil
}

func (d *spannedDeserializer) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	var value any
	var err error
	if start, ok := d.start.Take().Get(); ok {
		value, err = seed.DeserializeSeed(usizeDeserializerNew(start))
	} else if end, ok := d.end.Take().Get(); ok {
		value, err = seed.DeserializeSeed(usizeDeserializerNew(end))
	} else if inner, ok := d.value.Take().Get(); ok {
		value, err = seed.DeserializeSeed(inner)
	} else {
		panic(rs.Panicf("next_value_seed called before next_key_seed"))
	}
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *spannedDeserializer) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(d, keySeed, valueSeed)
}

func (d *spannedDeserializer) SizeHint() rs.Option[int] { return noSizeHint() }
