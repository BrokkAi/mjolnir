// Port of alloc 1.99.1 src/collections/btree/map/entry.rs

package collections

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// BTreeMapEntry is Rust's std::collections::btree_map::Entry: a view into a
// single entry of a map, which is either vacant or occupied. BTreeMap.Entry
// returns one. It is an enum in the port's usual form; a `match` on it is a
// type switch over *BTreeMapEntry_Vacant and *BTreeMapEntry_Occupied.
//
// An entry is a short-lived handle: use it before the map changes in any
// other way.
type BTreeMapEntry[K, V any] interface {
	isBTreeMapEntry()

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
	AndModify(f func(*V)) BTreeMapEntry[K, V]
	// InsertEntry is Entry::insert_entry: it sets the value of the entry and
	// returns it as an occupied entry.
	InsertEntry(value V) BTreeMapOccupiedEntry[K, V]
}

// BTreeMapEntry_Vacant is Entry::Vacant.
type BTreeMapEntry_Vacant[K, V any] struct{ F0 BTreeMapVacantEntry[K, V] }

// BTreeMapEntry_Occupied is Entry::Occupied.
type BTreeMapEntry_Occupied[K, V any] struct{ F0 BTreeMapOccupiedEntry[K, V] }

func (*BTreeMapEntry_Vacant[K, V]) isBTreeMapEntry()   {}
func (*BTreeMapEntry_Occupied[K, V]) isBTreeMapEntry() {}

// BTreeMapVacantEntry is Rust's btree_map::VacantEntry: the place in a map
// where a key that is not present would go.
type BTreeMapVacantEntry[K, V any] struct {
	m   *BTreeMap[K, V]
	key K
	// parent and right are where the node for key attaches, as BTreeMap.Entry
	// found it.
	parent *btreeNode[K, V]
	right  bool
}

// BTreeMapOccupiedEntry is Rust's btree_map::OccupiedEntry: an entry that is
// present in a map.
type BTreeMapOccupiedEntry[K, V any] struct {
	m    *BTreeMap[K, V]
	node *btreeNode[K, V]
}

// OrInsert is Entry::or_insert.
func (e *BTreeMapEntry_Occupied[K, V]) OrInsert(def V) *V { return e.F0.IntoMut() }

// OrInsert is Entry::or_insert.
func (e *BTreeMapEntry_Vacant[K, V]) OrInsert(def V) *V { return e.F0.Insert(def) }

// OrInsertWith is Entry::or_insert_with.
func (e *BTreeMapEntry_Occupied[K, V]) OrInsertWith(def func() V) *V { return e.F0.IntoMut() }

// OrInsertWith is Entry::or_insert_with.
func (e *BTreeMapEntry_Vacant[K, V]) OrInsertWith(def func() V) *V { return e.F0.Insert(def()) }

// OrInsertWithKey is Entry::or_insert_with_key.
func (e *BTreeMapEntry_Occupied[K, V]) OrInsertWithKey(def func(K) V) *V { return e.F0.IntoMut() }

// OrInsertWithKey is Entry::or_insert_with_key.
func (e *BTreeMapEntry_Vacant[K, V]) OrInsertWithKey(def func(K) V) *V {
	value := def(e.F0.Key())
	return e.F0.Insert(value)
}

// OrDefault is Entry::or_default.
func (e *BTreeMapEntry_Occupied[K, V]) OrDefault() *V { return e.F0.IntoMut() }

// OrDefault is Entry::or_default.
func (e *BTreeMapEntry_Vacant[K, V]) OrDefault() *V { return e.F0.Insert(rs.Default[V]()) }

// Key is Entry::key.
func (e *BTreeMapEntry_Occupied[K, V]) Key() K { return e.F0.Key() }

// Key is Entry::key.
func (e *BTreeMapEntry_Vacant[K, V]) Key() K { return e.F0.Key() }

// AndModify is Entry::and_modify.
func (e *BTreeMapEntry_Occupied[K, V]) AndModify(f func(*V)) BTreeMapEntry[K, V] {
	f(e.F0.GetMut())
	return e
}

// AndModify is Entry::and_modify.
func (e *BTreeMapEntry_Vacant[K, V]) AndModify(f func(*V)) BTreeMapEntry[K, V] { return e }

// InsertEntry is Entry::insert_entry.
func (e *BTreeMapEntry_Occupied[K, V]) InsertEntry(value V) BTreeMapOccupiedEntry[K, V] {
	e.F0.Insert(value)
	return e.F0
}

// InsertEntry is Entry::insert_entry.
func (e *BTreeMapEntry_Vacant[K, V]) InsertEntry(value V) BTreeMapOccupiedEntry[K, V] {
	return e.F0.InsertEntry(value)
}

// Key is VacantEntry::key: the key that would be used when inserting a value
// through the entry.
func (e BTreeMapVacantEntry[K, V]) Key() K { return e.key }

// IntoKey is VacantEntry::into_key: it takes ownership of the key.
func (e BTreeMapVacantEntry[K, V]) IntoKey() K { return e.key }

// Insert is VacantEntry::insert: it stores value under the entry's key and
// returns a pointer to it in the map.
func (e BTreeMapVacantEntry[K, V]) Insert(value V) *V {
	return &e.insert(value).value
}

// InsertEntry is VacantEntry::insert_entry: it stores value under the entry's
// key and returns the entry, now occupied.
func (e BTreeMapVacantEntry[K, V]) InsertEntry(value V) BTreeMapOccupiedEntry[K, V] {
	return BTreeMapOccupiedEntry[K, V]{m: e.m, node: e.insert(value)}
}

func (e BTreeMapVacantEntry[K, V]) insert(value V) *btreeNode[K, V] {
	parent, right := e.parent, e.right
	// Rust's borrow of the map keeps it unchanged between Entry and this
	// call. Go cannot enforce that, so a place that is no longer free is
	// looked up again instead of being trusted.
	if !e.placeIsFree() {
		found, newParent, newRight := e.m.locate(e.key)
		if found != nil {
			found.value = value
			return found
		}
		parent, right = newParent, newRight
	}
	node := &btreeNode[K, V]{key: e.key, value: value, left: nil, right: nil, parent: nil, height: 1}
	e.m.link(node, parent, right)
	return node
}

func (e BTreeMapVacantEntry[K, V]) placeIsFree() bool {
	switch {
	case e.parent == nil:
		return e.m.root == nil
	case e.parent.height == 0:
		return false
	case e.right:
		return e.parent.right == nil
	default:
		return e.parent.left == nil
	}
}

// Key is OccupiedEntry::key: the key stored in the entry.
func (e BTreeMapOccupiedEntry[K, V]) Key() K { return e.node.key }

// RemoveEntry is OccupiedEntry::remove_entry: it takes the entry out of the
// map and returns its key and value.
func (e BTreeMapOccupiedEntry[K, V]) RemoveEntry() (K, V) {
	e.removeKv()
	return e.node.key, e.node.value
}

// Get is OccupiedEntry::get: a pointer to the value in the entry.
func (e BTreeMapOccupiedEntry[K, V]) Get() *V { return &e.node.value }

// GetMut is OccupiedEntry::get_mut: a pointer to the value in the entry.
func (e BTreeMapOccupiedEntry[K, V]) GetMut() *V { return &e.node.value }

// IntoMut is OccupiedEntry::into_mut: a pointer to the value in the entry
// that may outlive the entry.
func (e BTreeMapOccupiedEntry[K, V]) IntoMut() *V { return &e.node.value }

// Insert is OccupiedEntry::insert: it sets the value of the entry and
// returns the old one.
func (e BTreeMapOccupiedEntry[K, V]) Insert(value V) V {
	old := e.node.value
	e.node.value = value
	return old
}

// Remove is OccupiedEntry::remove: it takes the entry out of the map and
// returns its value.
func (e BTreeMapOccupiedEntry[K, V]) Remove() V {
	e.removeKv()
	return e.node.value
}

// removeKv is the body of remove_entry. An entry already removed (through a
// second handle to it) is left alone.
func (e BTreeMapOccupiedEntry[K, V]) removeKv() {
	if e.node.height != 0 {
		e.m.unlink(e.node)
	}
}
