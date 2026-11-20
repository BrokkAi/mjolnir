package serde

import (
	"reflect"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Serialize is `value.serialize(serializer)`: it writes value to the
// serializer as its Rust type's Serialize impl does, and returns the
// serializer's `Ok`.
//
// value is a Go value of a serializable type, or a pointer to one: a pointer
// stands for `&T`, `Box<T>` and `Arc<T>`, which all serialize as T. A nil
// pointer or interface is a porting error (nothing in Rust is nil) and
// panics.
func Serialize(value any, serializer Serializer) (any, error) {
	switch v := value.(type) {
	case nil:
		panic(rs.Panicf("serde: cannot serialize a nil value; nothing in Rust is nil"))
	case *serializeRef:
		return v.codec.ser(v.pointer, serializer)
	case Serializable:
		return v.SerializeSerde(serializer)
	case string:
		return serializer.SerializeStr(v)
	case bool:
		return serializer.SerializeBool(v)
	}
	typ := reflect.TypeOf(value)
	if typ.Kind() == reflect.Pointer {
		pointer := reflect.ValueOf(value).UnsafePointer()
		if pointer == nil {
			panic(rs.Panicf("serde: cannot serialize a nil " + typ.String() + "; nothing in Rust is nil"))
		}
		if c := variantPointerCodec(typ); c != nil {
			// A pointer to a variant struct is a value of its enum.
			return c.serVariant(typ.Elem(), pointer, serializer)
		}
		return codecOf(typ.Elem()).ser(pointer, serializer)
	}
	// The value is a copy held in the interface; give the codec its address.
	holder := reflect.New(typ)
	holder.Elem().Set(reflect.ValueOf(value))
	return codecOf(typ).ser(holder.UnsafePointer(), serializer)
}

// SerializeRef is Serialize for a pointer whose type is known statically. It
// skips the copy Serialize makes of a value passed directly.
func SerializeRef[T any](value *T, serializer Serializer) (any, error) {
	if value == nil {
		panic(rs.Panicf("serde: cannot serialize a nil pointer; nothing in Rust is nil"))
	}
	return codecOf(reflect.TypeFor[T]()).ser(unsafe.Pointer(value), serializer)
}

// serializeRef is how the engine hands a field, an element or a key to a
// Serializer: the address of the value with the codec of its type. Serialize
// recognises it and skips the lookup by type.
type serializeRef struct {
	codec   *codec
	pointer unsafe.Pointer
}

// Deserialize is `T::deserialize(deserializer)`: it reads a T from the
// deserializer as the Rust type's Deserialize impl does.
func Deserialize[T any](deserializer Deserializer) (T, error) {
	var out T
	if err := codecOf(reflect.TypeFor[T]()).de(unsafe.Pointer(&out), deserializer); err != nil {
		var zero T
		return zero, err
	}
	return out, nil
}

// DeserializeInto is Deserialize for a destination whose type is known only
// at run time: out is a non-nil pointer to the value to fill.
func DeserializeInto(deserializer Deserializer, out any) error {
	value := reflect.ValueOf(out)
	if value.Kind() != reflect.Pointer || value.IsNil() {
		panic(rs.Panicf("serde: DeserializeInto needs a non-nil pointer"))
	}
	return codecOf(value.Type().Elem()).de(value.UnsafePointer(), deserializer)
}

// SeedFor is the seed Rust writes as `PhantomData<T>`: a DeDeserializeSeed
// that reads a T with its Deserialize impl. It stores the value in *out and
// returns nil as its value.
func SeedFor[T any](out *T) DeDeserializeSeed {
	return &placeSeed{codec: codecOf(reflect.TypeFor[T]()), pointer: unsafe.Pointer(out)}
}

// placeSeed deserializes one value of the codec's type into pointer.
type placeSeed struct {
	codec   *codec
	pointer unsafe.Pointer
}

func (s *placeSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	return nil, s.codec.de(s.pointer, deserializer)
}

// DeSeqAccessNextElement is `seq.next_element::<T>()`.
func DeSeqAccessNextElement[T any](seq DeSeqAccess) (rs.Option[T], error) {
	var out T
	_, ok, err := seq.NextElementSeed(SeedFor(&out))
	if err != nil || !ok {
		return rs.None[T](), err
	}
	return rs.Some(out), nil
}

// DeMapAccessNextKey is `map.next_key::<K>()`.
func DeMapAccessNextKey[K any](m DeMapAccess) (rs.Option[K], error) {
	var out K
	_, ok, err := m.NextKeySeed(SeedFor(&out))
	if err != nil || !ok {
		return rs.None[K](), err
	}
	return rs.Some(out), nil
}

// DeMapAccessNextValue is `map.next_value::<V>()`.
func DeMapAccessNextValue[V any](m DeMapAccess) (V, error) {
	var out V
	if _, err := m.NextValueSeed(SeedFor(&out)); err != nil {
		var zero V
		return zero, err
	}
	return out, nil
}

// DeMapAccessNextEntry is `map.next_entry::<K, V>()`.
func DeMapAccessNextEntry[K any, V any](m DeMapAccess) (rs.Option[rs.Tuple2[K, V]], error) {
	var key K
	var value V
	_, _, ok, err := m.NextEntrySeed(SeedFor(&key), SeedFor(&value))
	if err != nil || !ok {
		return rs.None[rs.Tuple2[K, V]](), err
	}
	return rs.Some(rs.T2(key, value)), nil
}

// DeEnumAccessVariant is `data.variant::<V>()`: the variant's identifier,
// read as a V, and the access to the variant's content.
func DeEnumAccessVariant[V any](data DeEnumAccess) (V, DeVariantAccess, error) {
	var out V
	_, variant, err := data.VariantSeed(SeedFor(&out))
	if err != nil {
		var zero V
		return zero, nil, err
	}
	return out, variant, nil
}

// DeVariantAccessNewtypeVariant is `variant.newtype_variant::<T>()`.
func DeVariantAccessNewtypeVariant[T any](variant DeVariantAccess) (T, error) {
	var out T
	if _, err := variant.NewtypeVariantSeed(SeedFor(&out)); err != nil {
		var zero T
		return zero, err
	}
	return out, nil
}
