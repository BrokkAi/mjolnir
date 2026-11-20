// Port of std 1.99.1 src/collections/hash/map.rs. Rust's HashMap is
// hashbrown's table behind SipHash with a random key; here it is a Go map,
// which gives the same contract: no order, one entry for each key.

package collections

import (
	"iter"
	"reflect"
	"slices"
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// HashMap is Rust's HashMap<K, V>: a map without an order.
//
// The zero value is an empty map, the same as HashMapNew. It is changed
// through a pointer, and a plain assignment shares the storage instead of
// copying it (the translation of a Rust move); `map.clone()` is rs.Clone(m),
// which rebuilds the map through rs.ReflectMap as BTreeMap describes.
//
// Keys. Any Go type can be the key type, as any Rust type with Hash and Eq
// can. Two keys are the same key when Rust's Eq says so, which is decided in
// one of three ways, chosen once for the key type:
//
//   - A key type with a method `HashKey() string`, on the value or on the
//     pointer, stands for a Rust type with its own Hash and Eq (rule 2 of
//     the translation rules; rspath.Key is one, with `Equal` beside it).
//     The entries are held in a Go map by that string, so two keys are the
//     same key exactly when their HashKeys are equal, which is the
//     contract of HashKey.
//   - A key type that Go's == compares as Rust's derived Eq does, held in a
//     Go map by the key itself: bools, numbers, strings, data-free enums,
//     and arrays and structs of those (rs.Tuple and rs.Option of them
//     included), when the type has no Equal or Compare method of its own.
//   - Any other key type, held in a B-tree in order of rs.Cmp: one that Go
//     cannot compare at all (a slice for a Vec, as in
//     `HashMap<Vec<u32>, u32>`, a Go map, or a struct or array holding
//     one), and one that Go compares by address where Rust compares the
//     value (a pointer for a Box or an Arc, an interface for an enum whose
//     variants carry data, or a struct or array holding them), and a type
//     with its own Equal or Compare but no HashKey. Two keys are the same
//     key when rs.Cmp finds them equal, which for the derived Hash, Eq and
//     Ord of such types is what rs.Eq finds. A lookup is a logarithmic
//     search instead of a hash, and the key type needs no method, as a
//     derived Hash needs none.
//
// A Rust raw pointer (`*const T`) compares by address; hold it as a uintptr
// or an unsafe.Pointer. A key is stored as it is given, as Rust moves it into
// the map: a slice key must not be changed afterwards.
//
// Pointers into the map. Get, GetMut, Entry and the iterators hand out a *V
// that points at the value stored in the map. Each entry has its own heap
// cell, so such a pointer stays valid, and keeps pointing at that entry's
// value, until the entry is removed.
//
// Iteration order is not specified, as in Rust; it is Go's map order and
// changes from one loop to the next. IterSorted gives key order for callers
// that need the same order every time. Removing the entry just yielded
// while iterating is safe; any other change to the map during iteration is
// not supported.
//
// Not covered: capacity (Rust reports hashbrown's bucket arithmetic, and a
// Go map has none to report), the hasher parameter S and its constructors,
// try_reserve, get_disjoint_mut, and the unstable try_insert.
type HashMap[K any, V any] struct {
	// table holds the entries by key, for a key type Go's == compares as
	// Rust's Eq does. The keys are held as interface values because K need
	// not be comparable in Go; only a key type that is uses table.
	table map[any]*hashMapEntry[K, V]
	// keyed holds the entries by the HashKey of their key, for a key type
	// that has that method.
	keyed map[string]*hashMapEntry[K, V]
	// ordered holds the entries in order of rs.Cmp, for a key type Go's ==
	// does not compare as Rust does.
	ordered BTreeMap[K, *hashMapEntry[K, V]]
	// keying says which of the three is in use. It is decided when the
	// first entry is inserted.
	keying hashMapKeying
}

// hashMapEntry is the heap cell of one entry.
type hashMapEntry[K any, V any] struct {
	key   K
	value V
}

type hashMapKeying uint8

const (
	hashMapKeyingUnset hashMapKeying = iota
	hashMapKeyingNative
	hashMapKeyingHashKey
	hashMapKeyingOrdered
)

// hashKeyer is a key type with a hand-written Hash (translation rule 2).
type hashKeyer interface{ HashKey() string }

var (
	hashKeyerType   = reflect.TypeFor[hashKeyer]()
	hashKeyingCache sync.Map // reflect.Type -> hashMapKeying
)

// hashMapKeyingFor decides how keys of type K are told apart.
func hashMapKeyingFor[K any]() hashMapKeying {
	keyType := reflect.TypeFor[K]()
	if cached, ok := hashKeyingCache.Load(keyType); ok {
		return cached.(hashMapKeying)
	}
	var keying hashMapKeying
	switch {
	case keyType.Implements(hashKeyerType) || reflect.PointerTo(keyType).Implements(hashKeyerType):
		keying = hashMapKeyingHashKey
	case hashKeyIsPlain(keyType):
		keying = hashMapKeyingNative
	default:
		keying = hashMapKeyingOrdered
	}
	hashKeyingCache.Store(keyType, keying)
	return keying
}

// hashKeyIsPlain reports whether Go's == on a value of the type is Rust's
// derived Eq, so that the key can index a Go map. rs.EqIsGoEqual decides it,
// since it is rs.Eq's own answer: no pointer, interface, slice or map inside
// the type, no hand-written Equal, and no field whose tag names a comparer
// (a path field compares by component, which == does not). A type with a
// hand-written Compare is kept ordered as well, since rs.Cmp asks it.
func hashKeyIsPlain(keyType reflect.Type) bool {
	if _, found := reflect.PointerTo(keyType).MethodByName("Compare"); found {
		return false
	}
	return rs.EqIsGoEqual(keyType)
}

// hashKeyOf returns the HashKey of a key of a type keyed that way.
func hashKeyOf[K any](key K) string {
	if keyer, ok := any(key).(hashKeyer); ok {
		return keyer.HashKey()
	}
	if keyer, ok := any(&key).(hashKeyer); ok {
		return keyer.HashKey()
	}
	// Only a nil interface gets here: its type has the method, the nil
	// value has nothing to call it on. Nothing in Rust is nil.
	panic("collections: a nil key was used in a HashMap or HashSet")
}

// prepare makes the map ready to take an entry.
func (m *HashMap[K, V]) prepare(capacity int) {
	if m.keying == hashMapKeyingUnset {
		m.keying = hashMapKeyingFor[K]()
	}
	switch {
	case m.keying == hashMapKeyingNative && m.table == nil:
		m.table = make(map[any]*hashMapEntry[K, V], capacity)
	case m.keying == hashMapKeyingHashKey && m.keyed == nil:
		m.keyed = make(map[string]*hashMapEntry[K, V], capacity)
	}
}

// find returns the entry for key, or nil.
func (m HashMap[K, V]) find(key K) *hashMapEntry[K, V] {
	switch m.keying {
	case hashMapKeyingNative:
		return m.table[any(key)]
	case hashMapKeyingHashKey:
		return m.keyed[hashKeyOf(key)]
	case hashMapKeyingOrdered:
		if n := m.ordered.find(key); n != nil {
			return n.value
		}
		return nil
	default:
		// Nothing was ever inserted.
		return nil
	}
}

// store adds an entry whose key is not in the map.
func (m *HashMap[K, V]) store(entry *hashMapEntry[K, V]) {
	m.prepare(0)
	switch m.keying {
	case hashMapKeyingNative:
		m.table[any(entry.key)] = entry
	case hashMapKeyingHashKey:
		m.keyed[hashKeyOf(entry.key)] = entry
	default:
		m.ordered.Insert(entry.key, entry)
	}
}

// erase removes the entry of key.
func (m *HashMap[K, V]) erase(key K) {
	switch m.keying {
	case hashMapKeyingNative:
		delete(m.table, any(key))
	case hashMapKeyingHashKey:
		delete(m.keyed, hashKeyOf(key))
	case hashMapKeyingOrdered:
		m.ordered.Remove(key)
	case hashMapKeyingUnset:
	}
}

// replaceKey stores key as the key of entry, which holds a key equal to it:
// HashSet::replace.
func (m *HashMap[K, V]) replaceKey(entry *hashMapEntry[K, V], key K) {
	entry.key = key
	if m.keying == hashMapKeyingOrdered {
		m.ordered.find(key).key = key
	}
}

// each calls yield with every entry, in Go's map order (in rs.Cmp order for
// the keys kept that way). yield may erase the entry it was given.
func (m HashMap[K, V]) each(yield func(*hashMapEntry[K, V]) bool) {
	for _, entry := range m.table {
		if !yield(entry) {
			return
		}
	}
	for _, entry := range m.keyed {
		if !yield(entry) {
			return
		}
	}
	btreeAscend(m.ordered.firstNode(), nil, func(n *btreeNode[K, *hashMapEntry[K, V]]) bool { return yield(n.value) })
}

// takeAll empties the map and returns what it held.
func (m *HashMap[K, V]) takeAll() HashMap[K, V] {
	taken := *m
	m.table, m.keyed, m.ordered = nil, nil, BTreeMapNew[K, *hashMapEntry[K, V]]()
	return taken
}

// HashMapNew is HashMap::new: an empty map.
func HashMapNew[K any, V any]() HashMap[K, V] {
	var empty HashMap[K, V]
	return empty
}

// HashMapDefault is HashMap::default: an empty map.
func HashMapDefault[K any, V any]() HashMap[K, V] {
	var empty HashMap[K, V]
	return empty
}

// HashMapWithCapacity is HashMap::with_capacity: an empty map with space
// for at least capacity entries.
func HashMapWithCapacity[K any, V any](capacity int) HashMap[K, V] {
	m := HashMapNew[K, V]()
	if capacity > 0 {
		m.prepare(capacity)
	}
	return m
}

// HashMapFrom is HashMap::from([(k, v), ..]). Of several pairs with equal
// keys, the first key is kept with the last value.
func HashMapFrom[K any, V any](pairs ...rs.Tuple2[K, V]) HashMap[K, V] {
	m := HashMapNew[K, V]()
	for _, pair := range pairs {
		m.Insert(pair.F0, pair.F1)
	}
	return m
}

// HashMapFromIter is `iter.collect::<HashMap<K, V>>()` (FromIterator). Of
// several pairs with equal keys, the first key is kept with the last value.
func HashMapFromIter[K any, V any](seq iter.Seq2[K, V]) HashMap[K, V] {
	m := HashMapNew[K, V]()
	m.Extend(seq)
	return m
}

// Keys is HashMap::keys: every key, in no particular order.
func (m HashMap[K, V]) Keys() iter.Seq[K] {
	return func(yield func(K) bool) {
		m.each(func(entry *hashMapEntry[K, V]) bool { return yield(entry.key) })
	}
}

// IntoKeys is HashMap::into_keys: every key, in no particular order, for a
// map that is not used again.
func (m HashMap[K, V]) IntoKeys() iter.Seq[K] {
	return m.Keys()
}

// Values is HashMap::values: a pointer to every value, in no particular
// order.
func (m HashMap[K, V]) Values() iter.Seq[*V] {
	return func(yield func(*V) bool) {
		m.each(func(entry *hashMapEntry[K, V]) bool { return yield(&entry.value) })
	}
}

// ValuesMut is HashMap::values_mut. It is the same sequence as Values.
func (m *HashMap[K, V]) ValuesMut() iter.Seq[*V] {
	return HashMap[K, V].Values(*m)
}

// IntoValues is HashMap::into_values: every value, in no particular order,
// for a map that is not used again.
func (m HashMap[K, V]) IntoValues() iter.Seq[V] {
	return func(yield func(V) bool) {
		m.each(func(entry *hashMapEntry[K, V]) bool { return yield(entry.value) })
	}
}

// Iter is HashMap::iter (`for (k, v) in &map`): every entry in no
// particular order, as the key and a pointer to the value in the map.
func (m HashMap[K, V]) Iter() iter.Seq2[K, *V] {
	return func(yield func(K, *V) bool) {
		m.each(func(entry *hashMapEntry[K, V]) bool { return yield(entry.key, &entry.value) })
	}
}

// IterMut is HashMap::iter_mut. It is the same sequence as Iter.
func (m *HashMap[K, V]) IterMut() iter.Seq2[K, *V] {
	return HashMap[K, V].Iter(*m)
}

// IterSorted yields what Iter yields, in the order of the keys by rs.Cmp. It
// has no counterpart in Rust: it is for the callers whose Rust code depends
// on nothing but a consistent order, and whose output must be the same on
// every run.
func (m HashMap[K, V]) IterSorted() iter.Seq2[K, *V] {
	return func(yield func(K, *V) bool) {
		entries := make([]*hashMapEntry[K, V], 0, m.Len())
		m.each(func(entry *hashMapEntry[K, V]) bool {
			entries = append(entries, entry)
			return true
		})
		slices.SortFunc(entries, func(a, b *hashMapEntry[K, V]) int { return rs.Cmp(a.key, b.key) })
		for _, entry := range entries {
			if !yield(entry.key, &entry.value) {
				return
			}
		}
	}
}

// IntoIter is HashMap::into_iter (`for (k, v) in map`): every entry in no
// particular order with its value moved out, for a map that is not used
// again.
func (m HashMap[K, V]) IntoIter() iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		m.each(func(entry *hashMapEntry[K, V]) bool { return yield(entry.key, entry.value) })
	}
}

// Len is HashMap::len: the number of entries.
func (m HashMap[K, V]) Len() int {
	return len(m.table) + len(m.keyed) + m.ordered.Len()
}

// IsEmpty is HashMap::is_empty.
func (m HashMap[K, V]) IsEmpty() bool {
	return m.Len() == 0
}

// Drain is HashMap::drain: it clears the map, returning all key-value pairs
// in no particular order. The map is empty by the time Drain returns; in
// Rust it cannot be touched until the iterator is dropped, and is empty
// then.
func (m *HashMap[K, V]) Drain() iter.Seq2[K, V] {
	drained := m.takeAll()
	return drained.IntoIter()
}

// ExtractIf is HashMap::extract_if: a sequence that visits every entry in
// no particular order and removes and yields those for which pred returns
// true. It is lazy as in Rust: entries are removed as the sequence is
// consumed, and an entry not reached stays in the map.
func (m *HashMap[K, V]) ExtractIf(pred func(K, *V) bool) iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		m.each(func(entry *hashMapEntry[K, V]) bool {
			if pred(entry.key, &entry.value) {
				m.erase(entry.key)
				return yield(entry.key, entry.value)
			}
			return true
		})
	}
}

// Retain is HashMap::retain: it keeps the entries for which f returns true,
// visiting them in no particular order. f may change the value through the
// pointer.
func (m *HashMap[K, V]) Retain(f func(K, *V) bool) {
	m.each(func(entry *hashMapEntry[K, V]) bool {
		if !f(entry.key, &entry.value) {
			m.erase(entry.key)
		}
		return true
	})
}

// Clear is HashMap::clear: it removes every entry.
func (m *HashMap[K, V]) Clear() {
	clear(m.table)
	clear(m.keyed)
	m.ordered.Clear()
}

// Reserve is HashMap::reserve. A Go map grows on its own, so this does
// nothing.
func (m *HashMap[K, V]) Reserve(additional int) {}

// ShrinkToFit is HashMap::shrink_to_fit. A Go map does not give memory
// back, so this does nothing.
func (m *HashMap[K, V]) ShrinkToFit() {}

// ShrinkTo is HashMap::shrink_to. A Go map does not give memory back, so
// this does nothing.
func (m *HashMap[K, V]) ShrinkTo(minCapacity int) {}

// Entry is HashMap::entry: the place of key in the map, for inserting or
// changing in one lookup. The entry must be used before the map is changed
// in any other way. See BTreeMap.Entry for how it is used.
func (m *HashMap[K, V]) Entry(key K) HashMapEntry[K, V] {
	if entry := m.find(key); entry != nil {
		return &HashMapEntry_Occupied[K, V]{F0: HashMapOccupiedEntry[K, V]{m: m, entry: entry}}
	}
	return &HashMapEntry_Vacant[K, V]{F0: HashMapVacantEntry[K, V]{m: m, key: key}}
}

// Get is HashMap::get: a pointer to the value stored for key, and whether
// there is one. The pointer is nil when the key is absent.
func (m HashMap[K, V]) Get(key K) (*V, bool) {
	if entry := m.find(key); entry != nil {
		return &entry.value, true
	}
	return nil, false
}

// GetKeyValue is HashMap::get_key_value: the stored key and a pointer to
// its value.
func (m HashMap[K, V]) GetKeyValue(key K) (K, *V, bool) {
	if entry := m.find(key); entry != nil {
		return entry.key, &entry.value, true
	}
	var zero K
	return zero, nil, false
}

// ContainsKey is HashMap::contains_key.
func (m HashMap[K, V]) ContainsKey(key K) bool {
	return m.find(key) != nil
}

// GetMut is HashMap::get_mut. It is the same pointer Get returns.
func (m *HashMap[K, V]) GetMut(key K) (*V, bool) {
	if entry := m.find(key); entry != nil {
		return &entry.value, true
	}
	return nil, false
}

// Insert is HashMap::insert: it stores value under key and returns the value
// the key had. When the key is already present the stored key is kept.
func (m *HashMap[K, V]) Insert(key K, value V) rs.Option[V] {
	if entry := m.find(key); entry != nil {
		old := entry.value
		entry.value = value
		return rs.Some(old)
	}
	m.store(&hashMapEntry[K, V]{key: key, value: value})
	return rs.None[V]()
}

// Remove is HashMap::remove: it removes key and returns the value it had.
func (m *HashMap[K, V]) Remove(key K) rs.Option[V] {
	if entry := m.find(key); entry != nil {
		m.erase(key)
		return rs.Some(entry.value)
	}
	return rs.None[V]()
}

// RemoveEntry is HashMap::remove_entry: it removes key and returns the
// stored key with its value.
func (m *HashMap[K, V]) RemoveEntry(key K) rs.Option[rs.Tuple2[K, V]] {
	if entry := m.find(key); entry != nil {
		m.erase(key)
		return rs.Some(rs.Tuple2[K, V]{F0: entry.key, F1: entry.value})
	}
	return rs.None[rs.Tuple2[K, V]]()
}

// Equal is HashMap's PartialEq: the same number of entries, and for every
// key of m an equal value (by rs.Eq) under that key in other.
func (m HashMap[K, V]) Equal(other *HashMap[K, V]) bool {
	if m.Len() != other.Len() {
		return false
	}
	equal := true
	m.each(func(entry *hashMapEntry[K, V]) bool {
		theirs := other.find(entry.key)
		equal = theirs != nil && rs.Eq(entry.value, theirs.value)
		return equal
	})
	return equal
}

// Index is `map[&key]`: a pointer to the value stored for key. It panics as
// Rust does when the key is absent.
func (m HashMap[K, V]) Index(key K) *V {
	entry := m.find(key)
	if entry == nil {
		rs.PanicAt(1, "no entry found for key")
	}
	return &entry.value
}

// Extend is HashMap::extend: it inserts every pair of seq, as Insert does.
func (m *HashMap[K, V]) Extend(seq iter.Seq2[K, V]) {
	for key, value := range seq {
		m.Insert(key, value)
	}
}

// HashMapEntry is Rust's std::collections::hash_map::Entry: a view into a
// single entry of a map, which is either vacant or occupied. HashMap.Entry
// returns one. A `match` on it is a type switch over *HashMapEntry_Vacant
// and *HashMapEntry_Occupied.
type HashMapEntry[K any, V any] interface {
	isHashMapEntry()

	// OrInsert is Entry::or_insert: it makes sure the entry holds a value,
	// inserting def when it is vacant, and returns a pointer to the value in
	// the map.
	OrInsert(def V) *V
	// OrInsertWith is Entry::or_insert_with: as OrInsert, calling def for the
	// value only when the entry is vacant.
	OrInsertWith(def func() V) *V
	// OrInsertWithKey is Entry::or_insert_with_key: as OrInsertWith, with the
	// entry's key passed to def.
	OrInsertWithKey(def func(K) V) *V
	// OrDefault is Entry::or_default: as OrInsert with V's default value,
	// rs.Default[V](). For a V whose Default is written by hand, use
	// OrInsertWith(VDefault).
	OrDefault() *V
	// Key is Entry::key: the entry's key.
	Key() K
	// AndModify is Entry::and_modify: it calls f with a pointer to the value
	// when the entry is occupied, and returns the entry.
	AndModify(f func(*V)) HashMapEntry[K, V]
	// InsertEntry is Entry::insert_entry: it sets the value of the entry and
	// returns it as an occupied entry.
	InsertEntry(value V) HashMapOccupiedEntry[K, V]
}

// HashMapEntry_Occupied is Entry::Occupied.
type HashMapEntry_Occupied[K any, V any] struct{ F0 HashMapOccupiedEntry[K, V] }

// HashMapEntry_Vacant is Entry::Vacant.
type HashMapEntry_Vacant[K any, V any] struct{ F0 HashMapVacantEntry[K, V] }

func (*HashMapEntry_Occupied[K, V]) isHashMapEntry() {}
func (*HashMapEntry_Vacant[K, V]) isHashMapEntry()   {}

// HashMapOccupiedEntry is Rust's hash_map::OccupiedEntry: an entry that is
// present in a map.
type HashMapOccupiedEntry[K any, V any] struct {
	m     *HashMap[K, V]
	entry *hashMapEntry[K, V]
}

// HashMapVacantEntry is Rust's hash_map::VacantEntry: the place in a map
// where a key that is not present would go.
type HashMapVacantEntry[K any, V any] struct {
	m   *HashMap[K, V]
	key K
}

// OrInsert is Entry::or_insert.
func (e *HashMapEntry_Occupied[K, V]) OrInsert(def V) *V { return e.F0.IntoMut() }

// OrInsert is Entry::or_insert.
func (e *HashMapEntry_Vacant[K, V]) OrInsert(def V) *V { return e.F0.Insert(def) }

// OrInsertWith is Entry::or_insert_with.
func (e *HashMapEntry_Occupied[K, V]) OrInsertWith(def func() V) *V { return e.F0.IntoMut() }

// OrInsertWith is Entry::or_insert_with.
func (e *HashMapEntry_Vacant[K, V]) OrInsertWith(def func() V) *V { return e.F0.Insert(def()) }

// OrInsertWithKey is Entry::or_insert_with_key.
func (e *HashMapEntry_Occupied[K, V]) OrInsertWithKey(def func(K) V) *V { return e.F0.IntoMut() }

// OrInsertWithKey is Entry::or_insert_with_key.
func (e *HashMapEntry_Vacant[K, V]) OrInsertWithKey(def func(K) V) *V {
	value := def(e.F0.Key())
	return e.F0.Insert(value)
}

// OrDefault is Entry::or_default.
func (e *HashMapEntry_Occupied[K, V]) OrDefault() *V { return e.F0.IntoMut() }

// OrDefault is Entry::or_default.
func (e *HashMapEntry_Vacant[K, V]) OrDefault() *V { return e.F0.Insert(rs.Default[V]()) }

// Key is Entry::key.
func (e *HashMapEntry_Occupied[K, V]) Key() K { return e.F0.Key() }

// Key is Entry::key.
func (e *HashMapEntry_Vacant[K, V]) Key() K { return e.F0.Key() }

// AndModify is Entry::and_modify.
func (e *HashMapEntry_Occupied[K, V]) AndModify(f func(*V)) HashMapEntry[K, V] {
	f(e.F0.GetMut())
	return e
}

// AndModify is Entry::and_modify.
func (e *HashMapEntry_Vacant[K, V]) AndModify(f func(*V)) HashMapEntry[K, V] { return e }

// InsertEntry is Entry::insert_entry.
func (e *HashMapEntry_Occupied[K, V]) InsertEntry(value V) HashMapOccupiedEntry[K, V] {
	e.F0.Insert(value)
	return e.F0
}

// InsertEntry is Entry::insert_entry.
func (e *HashMapEntry_Vacant[K, V]) InsertEntry(value V) HashMapOccupiedEntry[K, V] {
	return e.F0.InsertEntry(value)
}

// Key is OccupiedEntry::key: the key stored in the entry.
func (e HashMapOccupiedEntry[K, V]) Key() K { return e.entry.key }

// RemoveEntry is OccupiedEntry::remove_entry: it takes the entry out of the
// map and returns its key and value.
func (e HashMapOccupiedEntry[K, V]) RemoveEntry() (K, V) {
	e.m.erase(e.entry.key)
	return e.entry.key, e.entry.value
}

// Get is OccupiedEntry::get: a pointer to the value in the entry.
func (e HashMapOccupiedEntry[K, V]) Get() *V { return &e.entry.value }

// GetMut is OccupiedEntry::get_mut: a pointer to the value in the entry.
func (e HashMapOccupiedEntry[K, V]) GetMut() *V { return &e.entry.value }

// IntoMut is OccupiedEntry::into_mut: a pointer to the value in the entry
// that may outlive the entry.
func (e HashMapOccupiedEntry[K, V]) IntoMut() *V { return &e.entry.value }

// Insert is OccupiedEntry::insert: it sets the value of the entry and
// returns the old one.
func (e HashMapOccupiedEntry[K, V]) Insert(value V) V {
	old := e.entry.value
	e.entry.value = value
	return old
}

// Remove is OccupiedEntry::remove: it takes the entry out of the map and
// returns its value.
func (e HashMapOccupiedEntry[K, V]) Remove() V {
	e.m.erase(e.entry.key)
	return e.entry.value
}

// Key is VacantEntry::key: the key that would be used when inserting a
// value through the entry.
func (e HashMapVacantEntry[K, V]) Key() K { return e.key }

// IntoKey is VacantEntry::into_key: it takes ownership of the key.
func (e HashMapVacantEntry[K, V]) IntoKey() K { return e.key }

// Insert is VacantEntry::insert: it stores value under the entry's key and
// returns a pointer to it in the map.
func (e HashMapVacantEntry[K, V]) Insert(value V) *V {
	return &e.insert(value).value
}

// InsertEntry is VacantEntry::insert_entry: it stores value under the
// entry's key and returns the entry, now occupied.
func (e HashMapVacantEntry[K, V]) InsertEntry(value V) HashMapOccupiedEntry[K, V] {
	return HashMapOccupiedEntry[K, V]{m: e.m, entry: e.insert(value)}
}

func (e HashMapVacantEntry[K, V]) insert(value V) *hashMapEntry[K, V] {
	// Rust's borrow of the map keeps the key absent between Entry and this
	// call. Go cannot enforce that, so the key is looked up again.
	if entry := e.m.find(e.key); entry != nil {
		entry.value = value
		return entry
	}
	entry := &hashMapEntry[K, V]{key: e.key, value: value}
	e.m.store(entry)
	return entry
}

// The methods below describe the map to rs's formatter, structural clone and
// comparison, and to go/crates/serde (rs.ReflectMap).

// ReflectLen returns the number of entries.
func (m *HashMap[K, V]) ReflectLen() int { return m.Len() }

// ReflectKeyType returns the type K.
func (m *HashMap[K, V]) ReflectKeyType() reflect.Type { return reflect.TypeFor[K]() }

// ReflectValueType returns the type V.
func (m *HashMap[K, V]) ReflectValueType() reflect.Type { return reflect.TypeFor[V]() }

// ReflectEntries calls yield with each key and a pointer to its value, in
// no particular order.
func (m *HashMap[K, V]) ReflectEntries(yield func(key any, value any) bool) {
	m.each(func(entry *hashMapEntry[K, V]) bool { return yield(entry.key, &entry.value) })
}

// ReflectInsert stores value (a V) under key (a K), as Insert does.
func (m *HashMap[K, V]) ReflectInsert(key any, value any) {
	m.Insert(reflectCast[K](key), reflectCast[V](value))
}

// ReflectSorted reports that iteration is not in key order.
func (m *HashMap[K, V]) ReflectSorted() bool { return false }

var _ rs.ReflectMap = (*HashMap[int, int])(nil)
