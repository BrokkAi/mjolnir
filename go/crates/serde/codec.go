package serde

import (
	"reflect"
	"sync"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// serFn serializes the value at p, whose type is the codec's.
type serFn func(p unsafe.Pointer, serializer Serializer) (any, error)

// deFn deserializes a value of the codec's type into p, which holds the zero
// value or a value that may be overwritten.
type deFn func(p unsafe.Pointer, deserializer Deserializer) error

// codec is how one Go type is serialized and deserialized: the Rust type's
// Serialize and Deserialize impls, worked out once from the type's
// registration, its struct tags and its shape, and cached.
type codec struct {
	typ reflect.Type
	ser serFn
	de  deFn
	// serVariant serializes the enum value that is a pointer p to the variant
	// struct variant. Only the codec of an enum held as a Go interface has
	// it.
	serVariant func(variant reflect.Type, p unsafe.Pointer, serializer Serializer) (any, error)
}

var (
	// codecs holds the finished codecs by type.
	codecs sync.Map // reflect.Type -> *codec
	// buildMu serializes codec construction. building holds the codecs under
	// construction, which a recursive type refers to before they are
	// finished.
	buildMu  sync.Mutex
	building map[reflect.Type]*codec
)

// codecOf returns the codec of typ, building it on first use.
func codecOf(typ reflect.Type) *codec {
	if found, ok := codecs.Load(typ); ok {
		return found.(*codec)
	}
	buildMu.Lock()
	defer buildMu.Unlock()
	if found, ok := codecs.Load(typ); ok {
		return found.(*codec)
	}
	outermost := building == nil
	if outermost {
		building = map[reflect.Type]*codec{}
		defer func() { building = nil }()
	}
	c := codecLocked(typ)
	if outermost {
		// Publish only when the whole group is complete, so that no other
		// goroutine sees a codec whose fields are still being filled in.
		for builtType, built := range building {
			codecs.Store(builtType, built)
		}
	}
	return c
}

// codecLocked returns the codec of typ while buildMu is held. The codec of a
// type that is being built is returned unfinished: its ser and de are set by
// the time anything calls them.
func codecLocked(typ reflect.Type) *codec {
	if found, ok := codecs.Load(typ); ok {
		return found.(*codec)
	}
	if found, ok := building[typ]; ok {
		return found
	}
	c := &codec{typ: typ}
	building[typ] = c
	buildCodec(c)
	if c.ser == nil || c.de == nil {
		panic(rs.Panicf("serde: no codec was built for " + typ.String()))
	}
	return c
}

// variantPointerCodec returns the codec of the enum that the pointer type typ
// is a variant of, or nil when typ does not point to a registered variant
// struct.
func variantPointerCodec(typ reflect.Type) *codec {
	info := rs.TypeInfoOf(typ)
	if info == nil || info.Enum == nil || info.Enum.GoType == nil {
		return nil
	}
	if info.Enum.GoType.Kind() != reflect.Interface {
		return nil
	}
	return codecOf(info.Enum.GoType)
}

var (
	serializableType   = reflect.TypeFor[Serializable]()
	deserializableType = reflect.TypeFor[Deserializable]()
)

// customMu guards the hand-written impls registered for types that cannot
// carry methods.
var (
	customMu  sync.RWMutex
	customSer = map[reflect.Type]serFn{}
	customDe  = map[reflect.Type]deFn{}
)

// RegisterSerialize registers a hand-written `impl Serialize for T` for a
// type that cannot carry a SerializeSerde method: an enum held as a Go
// interface, or a type of another package. It returns true so that it can
// initialise a package variable.
//
//	var _ = serde.RegisterSerialize(func(value ThemeChoice, s serde.Serializer) (any, error) { ... })
func RegisterSerialize[T any](serialize func(value T, serializer Serializer) (any, error)) bool {
	typ := reflect.TypeFor[T]()
	customMu.Lock()
	defer customMu.Unlock()
	customSer[typ] = func(p unsafe.Pointer, serializer Serializer) (any, error) {
		return serialize(*(*T)(p), serializer)
	}
	return true
}

// RegisterDeserialize registers a hand-written `impl Deserialize for T` for
// a type that cannot carry a DeserializeSerde method: an enum held as a Go
// interface, or a type of another package. It returns true so that it can
// initialise a package variable.
//
//	var _ = serde.RegisterDeserialize(func(d serde.Deserializer) (ThemeChoice, error) { ... })
func RegisterDeserialize[T any](deserialize func(deserializer Deserializer) (T, error)) bool {
	typ := reflect.TypeFor[T]()
	customMu.Lock()
	defer customMu.Unlock()
	customDe[typ] = func(p unsafe.Pointer, deserializer Deserializer) error {
		value, err := deserialize(deserializer)
		if err != nil {
			return err
		}
		*(*T)(p) = value
		return nil
	}
	return true
}

// handWritten sets the codec's ser and de from the impls the type wrote by
// hand: registered functions first, then the SerializeSerde and
// DeserializeSerde methods. It leaves unset whichever the type did not
// write.
func handWritten(c *codec) {
	typ := c.typ
	customMu.RLock()
	c.ser = customSer[typ]
	c.de = customDe[typ]
	customMu.RUnlock()
	pointerType := reflect.PointerTo(typ)
	if c.ser == nil && typ.Kind() != reflect.Interface && pointerType.Implements(serializableType) {
		c.ser = func(p unsafe.Pointer, serializer Serializer) (any, error) {
			return reflect.NewAt(typ, p).Interface().(Serializable).SerializeSerde(serializer)
		}
	}
	if c.de == nil && typ.Kind() != reflect.Interface && pointerType.Implements(deserializableType) {
		c.de = func(p unsafe.Pointer, deserializer Deserializer) error {
			return reflect.NewAt(typ, p).Interface().(Deserializable).DeserializeSerde(deserializer)
		}
	}
}

// JSONBridge is how the engine serializes and deserializes a Go type that
// knows only JSON: one that implements json.Marshaler or json.Unmarshaler
// and has no serde impl of its own (the types of acp-go). go/crates/serdejson
// installs it; the engine cannot import that package.
type JSONBridge struct {
	// SerializeJSON writes the value that the JSON text holds to the
	// serializer. A serializer that writes compact JSON receives the text
	// unchanged.
	SerializeJSON func(text []byte, serializer Serializer) (any, error)
	// DeserializeJSON reads the next value from the deserializer and returns
	// it as JSON text. A deserializer that reads JSON text returns the text
	// of the value unchanged.
	DeserializeJSON func(deserializer Deserializer) ([]byte, error)
}

var jsonBridge JSONBridge

// SetJSONBridge installs the JSON bridge. go/crates/serdejson calls it when
// it is initialised.
func SetJSONBridge(bridge JSONBridge) { jsonBridge = bridge }
