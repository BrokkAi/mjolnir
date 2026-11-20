// Port of toml 0.8.2 src/map.rs

// Copyright 2017 Serde Developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

package toml

import (
	"iter"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/collections"
)

// A map of `String` to [Value].
//
// By default the map is backed by a [`BTreeMap`]. Enable the `preserve_order`
// feature of toml-rs to use [`IndexMap`] instead.
//
// Mjolnir builds toml without `preserve_order`, so the map is a BTreeMap:
// keys are kept and iterated in byte order.

// Table is toml::Table, which is toml::map::Map<String, Value>: a map of
// `String` to Value. The zero value is an empty map.
//
// Assigning a Table to another variable shares its entries (it is a Rust
// move); `table.clone()` is Clone.
type Table struct {
	m collections.BTreeMap[string, Value]
}

var _ = rs.RegisterStruct[Table]("Map")

// TableNew is Map::new: makes a new empty Map.
func TableNew() Table {
	return Table{m: collections.BTreeMapNew[string, Value]()}
}

// TableWithCapacity is Map::with_capacity: makes a new empty Map with the
// given initial capacity. A BTreeMap does not support with_capacity.
func TableWithCapacity(capacity int) Table {
	_ = capacity
	return Table{m: collections.BTreeMapNew[string, Value]()}
}

// Clear is Map::clear: clears the map, removing all values.
func (t *Table) Clear() {
	t.m.Clear()
}

// Get is Map::get: returns a reference to the value corresponding to the
// key.
func (t *Table) Get(key string) rs.Option[Value] {
	if v, ok := t.m.Get(key); ok {
		return rs.Some(*v)
	}
	return rs.None[Value]()
}

// ContainsKey is Map::contains_key: returns true if the map contains a value
// for the specified key.
func (t *Table) ContainsKey(key string) bool {
	return t.m.ContainsKey(key)
}

// GetMut is Map::get_mut: returns a mutable reference to the value
// corresponding to the key.
func (t *Table) GetMut(key string) rs.Option[*Value] {
	if v, ok := t.m.GetMut(key); ok {
		return rs.Some(v)
	}
	return rs.None[*Value]()
}

// Insert is Map::insert: inserts a key-value pair into the map.
//
// If the map did not have this key present, `None` is returned.
//
// If the map did have this key present, the value is updated, and the old
// value is returned. The key is not updated, though; this matters for
// types that can be `==` without being identical.
func (t *Table) Insert(k string, v Value) rs.Option[Value] {
	return t.m.Insert(k, v)
}

// Remove is Map::remove: removes a key from the map, returning the value at
// the key if the key was previously in the map.
func (t *Table) Remove(key string) rs.Option[Value] {
	return t.m.Remove(key)
}

// Retain is Map::retain: retains only the elements specified by the `keep`
// predicate.
//
// In other words, remove all pairs `(k, v)` for which `keep(&k, &mut v)`
// returns `false`.
//
// The elements are visited in iteration order.
func (t *Table) Retain(keep func(key string, value *Value) bool) {
	t.m.Retain(func(key string, value *Value) bool { return keep(key, value) })
}

// Entry is Map::entry: gets the given key's corresponding entry in the map
// for in-place manipulation.
func (t *Table) Entry(key string) MapEntry {
	switch entry := t.m.Entry(key).(type) {
	case *collections.BTreeMapEntry_Vacant[string, Value]:
		return &MapEntry_Vacant{F0: MapVacantEntry{vacant: entry.F0}}
	case *collections.BTreeMapEntry_Occupied[string, Value]:
		return &MapEntry_Occupied{F0: MapOccupiedEntry{occupied: entry.F0}}
	default:
		panic(rs.Unreachable(entry))
	}
}

// Len is Map::len: returns the number of elements in the map.
func (t *Table) Len() int {
	return t.m.Len()
}

// IsEmpty is Map::is_empty: returns true if the map contains no elements.
func (t *Table) IsEmpty() bool {
	return t.m.IsEmpty()
}

// Iter is Map::iter (and `IntoIterator for &Map`): gets an iterator over the
// entries of the map.
func (t *Table) Iter() iter.Seq2[string, Value] {
	return func(yield func(string, Value) bool) {
		for key, value := range t.m.Iter() {
			if !yield(key, *value) {
				return
			}
		}
	}
}

// IterMut is Map::iter_mut (and `IntoIterator for &mut Map`): gets a mutable
// iterator over the entries of the map.
func (t *Table) IterMut() iter.Seq2[string, *Value] {
	return t.m.IterMut()
}

// Keys is Map::keys: gets an iterator over the keys of the map.
func (t *Table) Keys() iter.Seq[string] {
	return t.m.Keys()
}

// Values is Map::values: gets an iterator over the values of the map.
func (t *Table) Values() iter.Seq[Value] {
	return func(yield func(Value) bool) {
		for value := range t.m.Values() {
			if !yield(*value) {
				return
			}
		}
	}
}

// TableDefault is `impl Default for Map`.
func TableDefault() Table {
	return TableNew()
}

// Clone is `impl Clone for Map`.
func (t *Table) Clone() Table {
	return Table{m: rs.Clone(t.m)}
}

// Equal is `impl PartialEq for Map`.
func (t *Table) Equal(other *Table) bool {
	return t.m.Equal(&other.m)
}

// Index is `impl Index<&Q> for Map`: the value at index, panicking with
// "no entry found for key" when there is none.
func (t *Table) Index(index string) Value {
	return *t.m.Index(index)
}

// IndexMut is `impl IndexMut<&Q> for Map`.
func (t *Table) IndexMut(index string) *Value {
	return t.GetMut(index).Expect("no entry found for key")
}

// FmtDebug is `impl Debug for Map`, which is the BTreeMap's: `{"a": ..}`.
func (t *Table) FmtDebug(f *rs.Formatter) error {
	debug := f.DebugMap()
	for key, value := range t.m.Iter() {
		debug.Entry(key, *value)
	}
	return debug.Finish()
}

// SerializeSerde is `impl Serialize for Map`.
func (t *Table) SerializeSerde(serializer serde.Serializer) (any, error) {
	m, err := serializer.SerializeMap(rs.Some(t.Len()))
	if err != nil {
		return nil, err
	}
	for k, v := range t.Iter() {
		if err := m.SerializeKey(k); err != nil {
			return nil, err
		}
		if err := m.SerializeValue(v); err != nil {
			return nil, err
		}
	}
	return m.End()
}

// DeserializeSerde is `impl Deserialize for Map`.
func (t *Table) DeserializeSerde(deserializer serde.Deserializer) error {
	value, err := deserializer.DeserializeMap(mapVisitor{})
	if err != nil {
		return err
	}
	*t = value.(Table)
	return nil
}

// mapVisitor is the Visitor of `impl Deserialize for Map`.
type mapVisitor struct{}

func (mapVisitor) Expecting() string { return "a map" }

func (mapVisitor) VisitUnit() (any, error) {
	return TableNew(), nil
}

func (mapVisitor) VisitMap(visitor serde.DeMapAccess) (any, error) {
	values := TableNew()

	for {
		entry, err := serde.DeMapAccessNextEntry[string, Value](visitor)
		if err != nil {
			return nil, err
		}
		pair, ok := entry.Get()
		if !ok {
			break
		}
		values.Insert(pair.F0, pair.F1)
	}

	return values, nil
}

// TableFromIter is `impl FromIterator<(String, Value)> for Map`.
func TableFromIter(pairs ...rs.Tuple2[string, Value]) Table {
	return Table{m: collections.BTreeMapFrom(pairs...)}
}

// Extend is `impl Extend<(String, Value)> for Map`.
func (t *Table) Extend(pairs iter.Seq2[string, Value]) {
	t.m.Extend(pairs)
}

// MapEntry is toml::map::Entry: a view into a single entry in a map, which
// may either be vacant or occupied. This enum is constructed from the
// `entry` method on `Map`. A value is a *MapEntry_Vacant or a
// *MapEntry_Occupied.
type MapEntry interface {
	isMapEntry()
	// Key is Entry::key: returns a reference to this entry's key.
	Key() string
	// OrInsert is Entry::or_insert: ensures a value is in the entry by
	// inserting the default if empty, and returns a mutable reference to
	// the value in the entry.
	OrInsert(def Value) *Value
	// OrInsertWith is Entry::or_insert_with: ensures a value is in the entry
	// by inserting the result of the default function if empty, and returns
	// a mutable reference to the value in the entry.
	OrInsertWith(def func() Value) *Value
}

// MapEntry_Vacant is Entry::Vacant: a vacant Entry.
type MapEntry_Vacant struct{ F0 MapVacantEntry }

// MapEntry_Occupied is Entry::Occupied: an occupied Entry.
type MapEntry_Occupied struct{ F0 MapOccupiedEntry }

func (*MapEntry_Vacant) isMapEntry()   {}
func (*MapEntry_Occupied) isMapEntry() {}

var _ = rs.RegisterEnum[MapEntry]("Entry",
	rs.Variant[MapEntry_Vacant]("Vacant", rs.ShapeTuple),
	rs.Variant[MapEntry_Occupied]("Occupied", rs.ShapeTuple),
)

// MapVacantEntry is toml::map::VacantEntry: a vacant Entry. It is part of
// the `Entry` enum.
type MapVacantEntry struct {
	vacant collections.BTreeMapVacantEntry[string, Value]
}

// MapOccupiedEntry is toml::map::OccupiedEntry: an occupied Entry. It is
// part of the `Entry` enum.
type MapOccupiedEntry struct {
	occupied collections.BTreeMapOccupiedEntry[string, Value]
}

func (e *MapEntry_Vacant) Key() string   { return e.F0.Key() }
func (e *MapEntry_Occupied) Key() string { return e.F0.Key() }

func (e *MapEntry_Vacant) OrInsert(def Value) *Value   { return e.F0.Insert(def) }
func (e *MapEntry_Occupied) OrInsert(def Value) *Value { return e.F0.IntoMut() }

func (e *MapEntry_Vacant) OrInsertWith(def func() Value) *Value   { return e.F0.Insert(def()) }
func (e *MapEntry_Occupied) OrInsertWith(def func() Value) *Value { return e.F0.IntoMut() }

// Key is VacantEntry::key: gets a reference to the key that would be used
// when inserting a value through the VacantEntry.
func (e *MapVacantEntry) Key() string {
	return e.vacant.Key()
}

// Insert is VacantEntry::insert: sets the value of the entry with the
// VacantEntry's key, and returns a mutable reference to it.
func (e *MapVacantEntry) Insert(value Value) *Value {
	return e.vacant.Insert(value)
}

// Key is OccupiedEntry::key: gets a reference to the key in the entry.
func (e *MapOccupiedEntry) Key() string {
	return e.occupied.Key()
}

// Get is OccupiedEntry::get: gets a reference to the value in the entry.
func (e *MapOccupiedEntry) Get() Value {
	return *e.occupied.Get()
}

// GetMut is OccupiedEntry::get_mut: gets a mutable reference to the value in
// the entry.
func (e *MapOccupiedEntry) GetMut() *Value {
	return e.occupied.GetMut()
}

// IntoMut is OccupiedEntry::into_mut: converts the entry into a mutable
// reference to its value.
func (e *MapOccupiedEntry) IntoMut() *Value {
	return e.occupied.IntoMut()
}

// Insert is OccupiedEntry::insert: sets the value of the entry with the
// `OccupiedEntry`'s key, and returns the entry's old value.
func (e *MapOccupiedEntry) Insert(value Value) Value {
	return e.occupied.Insert(value)
}

// Remove is OccupiedEntry::remove: takes the value of the entry out of the
// map, and returns it.
func (e *MapOccupiedEntry) Remove() Value {
	return e.occupied.Remove()
}

// IntoIter is `impl IntoIterator for Map`: the entries, moved out of the
// map, in key order.
func (t Table) IntoIter() iter.Seq2[string, Value] {
	return t.m.IntoIter()
}
