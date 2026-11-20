package rs

import "reflect"

// The formatter, the structural clone and comparison, and go/crates/serde
// walk values by reflection. The collections of go/rs/collections (and
// indexmap, imbl) are generic structs with private storage, so they describe
// themselves through these interfaces instead. Each collection implements one
// of them on its pointer type.

// ReflectMap is a map-like collection: BTreeMap, HashMap, IndexMap, OrdMap.
type ReflectMap interface {
	// ReflectLen returns the number of entries.
	ReflectLen() int
	// ReflectKeyType and ReflectValueType return the types K and V.
	ReflectKeyType() reflect.Type
	ReflectValueType() reflect.Type
	// ReflectEntries calls yield with each key (a K) and a pointer to its
	// value (a *V), in the collection's iteration order, until yield returns
	// false.
	ReflectEntries(yield func(key any, value any) bool)
	// ReflectInsert stores value (a V) under key (a K), as Insert does.
	ReflectInsert(key any, value any)
	// ReflectSorted reports whether iteration is in key order. Maps that are
	// not (HashMap) are written in sorted key order where output must be
	// deterministic.
	ReflectSorted() bool
}

// ReflectSeq is a sequence- or set-like collection: BTreeSet, HashSet,
// VecDeque.
type ReflectSeq interface {
	// ReflectLen returns the number of elements.
	ReflectLen() int
	// ReflectElemType returns the type T.
	ReflectElemType() reflect.Type
	// ReflectElements calls yield with a pointer to each element (a *T) in
	// iteration order, until yield returns false.
	ReflectElements(yield func(element any) bool)
	// ReflectPush adds element (a T): insert for a set, push_back for a
	// deque.
	ReflectPush(element any)
	// ReflectIsSet reports whether the collection is a set, which Debug
	// prints with braces.
	ReflectIsSet() bool
	// ReflectSorted reports whether iteration is in element order.
	ReflectSorted() bool
}
