// Port of toml_edit 0.20.2 src/de/datetime.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// datetimeDeserializer is DatetimeDeserializer: the one-entry map a datetime
// is handed to a visitor as, `{ "$__toml_private_datetime": "<datetime>" }`,
// which toml_datetime's Deserialize impl recognises.
type datetimeDeserializer struct {
	date rs.Option[Datetime]
}

func datetimeDeserializerNew(date Datetime) *datetimeDeserializer {
	return &datetimeDeserializer{date: rs.Some(date)}
}

func (d *datetimeDeserializer) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if d.date.IsSome() {
		key, err := seed.DeserializeSeed(borrowedStrDeserializerNew(DatetimeField))
		if err != nil {
			return nil, false, deErrorFrom(err)
		}
		return key, true, nil
	}
	return nil, false, nil
}

func (d *datetimeDeserializer) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	if date, ok := d.date.Take().Get(); ok {
		value, err := seed.DeserializeSeed(stringDeserializerNew(date.String()))
		if err != nil {
			return nil, deErrorFrom(err)
		}
		return value, nil
	}
	panic(rs.Panicf("next_value_seed called before next_key_seed"))
}

func (d *datetimeDeserializer) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(d, keySeed, valueSeed)
}

func (d *datetimeDeserializer) SizeHint() rs.Option[int] { return noSizeHint() }
