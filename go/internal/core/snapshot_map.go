// Port of mj-core/src/snapshot_map.rs.
//
// Immutable snapshots with copy-on-write updates to individual records.
//
// Go has no reference counts to tell a shared value from a unique one, so
// every edit copies what it changes (the map of record pointers, and the one
// record being edited) and leaves every previously published value intact.
// Copies of V are shallow: a record whose fields are slices or maps must not
// be mutated through those in place after it has been published.

package core

import (
	"cmp"
	"encoding/json"
	"iter"
	"maps"
	"slices"
	"strings"
)

// SharedSnapshot is an immutable publication whose copies and unchanged
// comparisons are cheap. Copy-on-write edits leave every previously
// published value intact. The wire representation is exactly the inner
// value, including for older readers. The zero value holds a zero T.
type SharedSnapshot[T any] struct {
	value *T
}

// NewSharedSnapshot publishes value (Rust's From<T>).
func NewSharedSnapshot[T any](value T) SharedSnapshot[T] {
	return SharedSnapshot[T]{value: &value}
}

func (s SharedSnapshot[T]) PtrEq(other SharedSnapshot[T]) bool {
	return s.value == other.value
}

// IntoInner is a copy of the published value.
func (s SharedSnapshot[T]) IntoInner() T {
	return *s.Get()
}

// Equal compares by identity first, then by eq.
func (s SharedSnapshot[T]) Equal(other SharedSnapshot[T], eq func(a, b *T) bool) bool {
	return s.PtrEq(other) || eq(s.Get(), other.Get())
}

// Get is the published value (Rust's Deref). Callers must not mutate it.
func (s SharedSnapshot[T]) Get() *T {
	if s.value == nil {
		return new(T)
	}
	return s.value
}

// Mut copies the published value and returns the copy for editing (Rust's
// DerefMut through Arc::make_mut).
func (s *SharedSnapshot[T]) Mut() *T {
	clone := *s.Get()
	s.value = &clone
	return s.value
}

// MarshalJSON writes the inner value (serde transparent).
func (s SharedSnapshot[T]) MarshalJSON() ([]byte, error) {
	return relayMarshalJSON(s.Get())
}

// UnmarshalJSON reads the inner value (serde transparent).
func (s *SharedSnapshot[T]) UnmarshalJSON(data []byte) error {
	var value T
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	s.value = &value
	return nil
}

// SnapshotMap is an ordered map whose copies share records. Mutating a
// record copies only that record, even while readers hold an older snapshot.
// It serializes as a JSON object (serde transparent). The zero value is an
// empty map.
type SnapshotMap[K cmp.Ordered, V any] struct {
	// Never mutated in place once another copy of the map may hold it.
	entries map[K]*V
}

func NewSnapshotMap[K cmp.Ordered, V any]() SnapshotMap[K, V] {
	return SnapshotMap[K, V]{}
}

// CollectSnapshotMap builds a map from pairs (Rust's FromIterator and From
// of an array).
func CollectSnapshotMap[K cmp.Ordered, V any](pairs iter.Seq2[K, V]) SnapshotMap[K, V] {
	var m SnapshotMap[K, V]
	m.Extend(pairs)
	return m
}

// own gives this copy a private map of record pointers to edit.
func (m *SnapshotMap[K, V]) own() {
	m.entries = maps.Clone(m.entries)
	if m.entries == nil {
		m.entries = map[K]*V{}
	}
}

func (m SnapshotMap[K, V]) sortedKeys() []K {
	return slices.Sorted(maps.Keys(m.entries))
}

func (m SnapshotMap[K, V]) Len() int { return len(m.entries) }

func (m SnapshotMap[K, V]) IsEmpty() bool { return len(m.entries) == 0 }

// Get is a copy of the record at key.
func (m SnapshotMap[K, V]) Get(key K) (V, bool) {
	if value, ok := m.entries[key]; ok {
		return *value, true
	}
	var zero V
	return zero, false
}

// GetShared retains one immutable record without copying its contents.
// Callers must not mutate it.
func (m SnapshotMap[K, V]) GetShared(key K) *V {
	return m.entries[key]
}

// GetMut copies the record at key into this map and returns it for editing.
func (m *SnapshotMap[K, V]) GetMut(key K) *V {
	value, ok := m.entries[key]
	if !ok {
		return nil
	}
	clone := *value
	m.own()
	m.entries[key] = &clone
	return &clone
}

func (m SnapshotMap[K, V]) ContainsKey(key K) bool {
	_, ok := m.entries[key]
	return ok
}

// Insert stores value at key and returns the previous record, if any.
func (m *SnapshotMap[K, V]) Insert(key K, value V) (V, bool) {
	previous, ok := m.InsertShared(key, value)
	if !ok {
		var zero V
		return zero, false
	}
	return *previous, true
}

// InsertShared replaces a record without copying the previous value for the
// caller.
func (m *SnapshotMap[K, V]) InsertShared(key K, value V) (*V, bool) {
	previous, ok := m.entries[key]
	m.own()
	m.entries[key] = &value
	return previous, ok
}

func (m *SnapshotMap[K, V]) Remove(key K) (V, bool) {
	previous := m.RemoveShared(key)
	if previous == nil {
		var zero V
		return zero, false
	}
	return *previous, true
}

// RemoveShared removes a record while retaining its shared contents for
// existing readers.
func (m *SnapshotMap[K, V]) RemoveShared(key K) *V {
	previous, ok := m.entries[key]
	if !ok {
		return nil
	}
	m.own()
	delete(m.entries, key)
	return previous
}

func (m *SnapshotMap[K, V]) Clear() {
	m.entries = nil
}

// All yields every key and its shared record in key order. Callers must not
// mutate the records.
func (m SnapshotMap[K, V]) All() iter.Seq2[K, *V] {
	return func(yield func(K, *V) bool) {
		for _, key := range m.sortedKeys() {
			if !yield(key, m.entries[key]) {
				return
			}
		}
	}
}

func (m SnapshotMap[K, V]) Keys() iter.Seq[K] {
	return slices.Values(m.sortedKeys())
}

// Values yields the shared records in key order. Callers must not mutate them.
func (m SnapshotMap[K, V]) Values() iter.Seq[*V] {
	return func(yield func(*V) bool) {
		for _, key := range m.sortedKeys() {
			if !yield(m.entries[key]) {
				return
			}
		}
	}
}

// Changes yields what is needed to turn this snapshot into next: each key
// whose record was added or changed with its new record, and each removed
// key with nil. Shared records are skipped without calling eq.
func (m SnapshotMap[K, V]) Changes(next SnapshotMap[K, V], eq func(a, b *V) bool) iter.Seq2[K, *V] {
	return func(yield func(K, *V) bool) {
		keys := maps.Keys(m.entries)
		union := slices.Collect(keys)
		for key := range next.entries {
			if _, ok := m.entries[key]; !ok {
				union = append(union, key)
			}
		}
		slices.Sort(union)
		for _, key := range union {
			old, inOld := m.entries[key]
			value, inNext := next.entries[key]
			switch {
			case !inNext:
				if !yield(key, nil) {
					return
				}
			case !inOld:
				if !yield(key, value) {
					return
				}
			case old != value && !eq(old, value):
				if !yield(key, value) {
					return
				}
			}
		}
	}
}

// IntoValues yields copies of the records in key order.
func (m SnapshotMap[K, V]) IntoValues() iter.Seq[V] {
	return func(yield func(V) bool) {
		for _, key := range m.sortedKeys() {
			if !yield(*m.entries[key]) {
				return
			}
		}
	}
}

// FirstValueMut is the first record, copied for editing; nil when empty.
func (m *SnapshotMap[K, V]) FirstValueMut() *V {
	if len(m.entries) == 0 {
		return nil
	}
	return m.GetMut(slices.Min(slices.Collect(maps.Keys(m.entries))))
}

func (m *SnapshotMap[K, V]) Entry(key K) SnapshotEntry[K, V] {
	return SnapshotEntry[K, V]{snapshotMap: m, key: key}
}

func (m *SnapshotMap[K, V]) Retain(keep func(K, *V) bool) {
	var removed []K
	for _, key := range m.sortedKeys() {
		if !keep(key, m.entries[key]) {
			removed = append(removed, key)
		}
	}
	if len(removed) == 0 {
		return
	}
	m.own()
	for _, key := range removed {
		delete(m.entries, key)
	}
}

// CopyEntry makes key hold source's entry when keep is true and removes it
// otherwise. An entry that already shares source's value is left alone, so
// the map stays shared with earlier versions of this map.
func (m *SnapshotMap[K, V]) CopyEntry(source SnapshotMap[K, V], key K, keep bool) {
	value, ok := source.entries[key]
	if !ok || !keep {
		m.RemoveShared(key)
		return
	}
	if m.entries[key] != value {
		m.own()
		m.entries[key] = value
	}
}

// Filtered selects a smaller view while sharing the retained records'
// contents.
func (m SnapshotMap[K, V]) Filtered(keep func(K, *V) bool) SnapshotMap[K, V] {
	filtered := SnapshotMap[K, V]{entries: map[K]*V{}}
	for key, value := range m.entries {
		if keep(key, value) {
			filtered.entries[key] = value
		}
	}
	return filtered
}

// Extend inserts every pair.
func (m *SnapshotMap[K, V]) Extend(pairs iter.Seq2[K, V]) {
	m.own()
	for key, value := range pairs {
		m.entries[key] = &value
	}
}

// Index is the shared record at key; it panics when the key is absent.
func (m SnapshotMap[K, V]) Index(key K) *V {
	value, ok := m.entries[key]
	if !ok {
		panic("snapshot map key is absent")
	}
	return value
}

// IndexMut is the record at key, copied for editing; it panics when the key
// is absent.
func (m *SnapshotMap[K, V]) IndexMut(key K) *V {
	value := m.GetMut(key)
	if value == nil {
		panic("snapshot map key is absent")
	}
	return value
}

// Equal compares two maps record by record (Rust's derived PartialEq).
func (m SnapshotMap[K, V]) Equal(other SnapshotMap[K, V], eq func(a, b *V) bool) bool {
	return maps.EqualFunc(m.entries, other.entries, func(a, b *V) bool { return a == b || eq(a, b) })
}

// MarshalJSON writes the map as a JSON object.
func (m SnapshotMap[K, V]) MarshalJSON() ([]byte, error) {
	if m.entries == nil {
		return []byte("{}"), nil
	}
	return relayMarshalJSON(m.entries)
}

// UnmarshalJSON reads a JSON object.
func (m *SnapshotMap[K, V]) UnmarshalJSON(data []byte) error {
	var entries map[K]*V
	if err := json.Unmarshal(data, &entries); err != nil {
		return err
	}
	m.entries = entries
	return nil
}

// SnapshotMapKeysWithPrefix yields the keys that begin with prefix, in order
// (Rust's keys_with_prefix on a String-keyed map).
func SnapshotMapKeysWithPrefix[V any](m SnapshotMap[string, V], prefix string) iter.Seq[string] {
	return func(yield func(string) bool) {
		for _, key := range m.sortedKeys() {
			if strings.HasPrefix(key, prefix) && !yield(key) {
				return
			}
		}
	}
}

type SnapshotEntry[K cmp.Ordered, V any] struct {
	snapshotMap *SnapshotMap[K, V]
	key         K
}

func (e SnapshotEntry[K, V]) OrInsertWith(value func() V) *V {
	if !e.snapshotMap.ContainsKey(e.key) {
		e.snapshotMap.Insert(e.key, value())
	}
	return e.snapshotMap.GetMut(e.key)
}
