// Port of serde_core 1.0.229 src/de/ignored_any.rs

package serde

// DeIgnoredAny is `serde::de::IgnoredAny`: an efficient way of discarding
// data from a deserializer. Deserializing one reads past a value of any
// type; it is also its own visitor, as in Rust.
type DeIgnoredAny struct{}

// DeserializeSerde is `impl Deserialize for IgnoredAny`.
func (*DeIgnoredAny) DeserializeSerde(deserializer Deserializer) error {
	_, err := deserializer.DeserializeIgnoredAny(DeIgnoredAny{})
	return err
}

// DeserializeSeed lets an IgnoredAny be used where Rust writes
// `PhantomData<IgnoredAny>`.
func (DeIgnoredAny) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIgnoredAny(DeIgnoredAny{})
}

func (DeIgnoredAny) Expecting() string { return "anything at all" }

func (DeIgnoredAny) VisitBool(bool) (any, error)   { return nil, nil }
func (DeIgnoredAny) VisitI64(int64) (any, error)   { return nil, nil }
func (DeIgnoredAny) VisitU64(uint64) (any, error)  { return nil, nil }
func (DeIgnoredAny) VisitF64(float64) (any, error) { return nil, nil }
func (DeIgnoredAny) VisitStr(string) (any, error)  { return nil, nil }
func (DeIgnoredAny) VisitNone() (any, error)       { return nil, nil }
func (DeIgnoredAny) VisitUnit() (any, error)       { return nil, nil }
func (DeIgnoredAny) VisitBytes([]byte) (any, error) {
	return nil, nil
}

func (DeIgnoredAny) VisitSome(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIgnoredAny(DeIgnoredAny{})
}

func (DeIgnoredAny) VisitNewtypeStruct(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIgnoredAny(DeIgnoredAny{})
}

func (DeIgnoredAny) VisitSeq(seq DeSeqAccess) (any, error) {
	for {
		_, ok, err := seq.NextElementSeed(DeIgnoredAny{})
		if err != nil {
			return nil, err
		}
		if !ok {
			// Gobble
			return nil, nil
		}
	}
}

func (DeIgnoredAny) VisitMap(m DeMapAccess) (any, error) {
	for {
		_, _, ok, err := m.NextEntrySeed(DeIgnoredAny{}, DeIgnoredAny{})
		if err != nil {
			return nil, err
		}
		if !ok {
			// Gobble
			return nil, nil
		}
	}
}

func (DeIgnoredAny) VisitEnum(data DeEnumAccess) (any, error) {
	_, variant, err := data.VariantSeed(DeIgnoredAny{})
	if err != nil {
		return nil, err
	}
	return variant.NewtypeVariantSeed(DeIgnoredAny{})
}
