// Port of toml_edit 0.20.2 src/ser/array.rs

package tomledit

import "github.com/BrokkAi/mjolnir/go/crates/serde"

// serSerializeValueArray is SerializeValueArray: the SerializeSeq,
// SerializeTuple, SerializeTupleVariant and SerializeTupleStruct of
// ValueSerializer. Its `Ok` is a Value (an array).
type serSerializeValueArray struct {
	values []Item
}

func serSerializeValueArrayNew() *serSerializeValueArray {
	return &serSerializeValueArray{values: nil}
}

func serSerializeValueArrayWithCapacity(length int) *serSerializeValueArray {
	return &serSerializeValueArray{values: make([]Item, 0, length)}
}

// SerializeElement is `SerializeSeq::serialize_element` (and
// `SerializeTuple::serialize_element`).
func (s *serSerializeValueArray) SerializeElement(value any) error {
	serialized, err := serializeValue(value, SerValueSerializerNew())
	if err != nil {
		return err
	}
	s.values = append(s.values, &Item_Value{F0: serialized})
	return nil
}

// End is `SerializeSeq::end` (and the end of the other three).
func (s *serSerializeValueArray) End() (any, error) {
	return ValueFromArray(arrayWithVec(s.values)), nil
}

// SerializeField is `SerializeTupleVariant::serialize_field` and
// `SerializeTupleStruct::serialize_field`.
func (s *serSerializeValueArray) SerializeField(value any) error {
	return s.SerializeElement(value)
}

var (
	_ serde.SerSerializeSeq          = (*serSerializeValueArray)(nil)
	_ serde.SerSerializeTuple        = (*serSerializeValueArray)(nil)
	_ serde.SerSerializeTupleVariant = (*serSerializeValueArray)(nil)
	_ serde.SerSerializeTupleStruct  = (*serSerializeValueArray)(nil)
)
