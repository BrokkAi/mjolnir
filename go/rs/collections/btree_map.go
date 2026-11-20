// Port of alloc 1.99.1 src/collections/btree/map.rs

package collections

import (
	"iter"
	"math/bits"
	"reflect"
	"slices"
	"sort"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// BTreeMap is Rust's BTreeMap<K, V>: an ordered map whose entries are kept
// and iterated in key order. Keys are ordered by rs.Cmp, and by nothing else.
//
// The zero value is an empty map, the same as BTreeMapNew. Methods that Rust
// declares with `&mut self` have pointer receivers, so a map is changed
// through a pointer to it (a struct field, a local, the pointer GetMut of an
// outer map returns). Assigning a map to another variable does not copy its
// entries: both values then share the storage, and changing one corrupts the
// other. That is what a Rust move leaves behind (the source is not used
// again), so a plain assignment is the right translation of a move, and
// `map.clone()` is rs.Clone(m) (or m.Clone()).
//
// Pointers into the map. Get, GetMut, Entry, the first and last accessors and
// the iterators hand out a *V that points at the value stored in the map, so
// `map.get_mut(k).unwrap().field = x` needs no writing back. Each entry lives
// in its own heap cell that is never moved or reused, so such a pointer stays
// valid, and keeps pointing at that entry's value, for as long as the entry
// is in the map: inserting or removing other entries, Retain, Append and
// SplitOff do not disturb it (an entry that Append or SplitOff moves to
// another map takes its cell along). Once the entry is removed the pointer
// still reads the value it had, but is no longer connected to any map. Rust
// allows none of this to be relied on, since its borrow rules end every
// reference at the next insertion or removal; the guarantee exists so that a
// translation which holds a pointer slightly longer than Rust would is still
// correct.
//
// Iteration. The iterators are Go range-over-func sequences that yield
// entries in key order: `for k, v := range m.Iter()`. Changing the set of
// entries of a map while it is being iterated is not supported, as in Rust;
// the one thing that is safe is removing the entry the loop was just given.
// An iteration that finds its next entry gone panics rather than stop early.
//
// Results. A lookup that returns references in Rust returns pointers and a
// found flag here (`v, ok := m.Get(k)`, `k, v, ok := m.FirstKeyValue()`);
// a method that moves a value out returns an rs.Option (`m.Remove(k)`,
// `m.PopFirst()`). Keys are passed and returned by value throughout.
type BTreeMap[K, V any] struct {
	root   *btreeNode[K, V]
	length int
	// rooted is Rust's `self.root.is_some()`. A map that has held an entry
	// keeps an (empty) root node when its last entry is removed, and Rust
	// checks the bounds of a range only when there is a root, so whether
	// `range(5..3)` on an empty map panics depends on it.
	rooted bool
}

// BTreeMapNew is BTreeMap::new: an empty map.
func BTreeMapNew[K, V any]() BTreeMap[K, V] {
	var empty BTreeMap[K, V]
	return empty
}

// BTreeMapDefault is BTreeMap::default: an empty map.
func BTreeMapDefault[K, V any]() BTreeMap[K, V] {
	var empty BTreeMap[K, V]
	return empty
}

// BTreeMapFrom is BTreeMap::from([(k, v), ..]). Of several pairs with equal
// keys the last one is kept, key and value both.
func BTreeMapFrom[K, V any](pairs ...rs.Tuple2[K, V]) BTreeMap[K, V] {
	if len(pairs) == 0 {
		return BTreeMapNew[K, V]()
	}
	return btreeBulkBuildFromSortedIter(btreeSortPairs(slices.Clone(pairs)))
}

// BTreeMapFromIter is `iter.collect::<BTreeMap<K, V>>()` (FromIterator). Of
// several pairs with equal keys the last one is kept, key and value both;
// that differs from inserting the pairs one by one, which keeps the first
// key.
func BTreeMapFromIter[K, V any](seq iter.Seq2[K, V]) BTreeMap[K, V] {
	var inputs []rs.Tuple2[K, V]
	for key, value := range seq {
		inputs = append(inputs, rs.Tuple2[K, V]{F0: key, F1: value})
	}
	if len(inputs) == 0 {
		return BTreeMapNew[K, V]()
	}
	return btreeBulkBuildFromSortedIter(btreeSortPairs(inputs))
}

// btreeSortPairs sorts pairs by key. Rust says: use stable sort to preserve
// the insertion order.
func btreeSortPairs[K, V any](pairs []rs.Tuple2[K, V]) []rs.Tuple2[K, V] {
	slices.SortStableFunc(pairs, func(a, b rs.Tuple2[K, V]) int { return rs.Cmp(a.F0, b.F0) })
	return pairs
}

// btreeBulkBuildFromSortedIter is BTreeMap::bulk_build_from_sorted_iter over
// DedupSortedIter (dedup_sorted_iter.rs): when it meets a run of equal keys,
// only the last pair of the run is kept. The result has a root even when it
// is empty.
func btreeBulkBuildFromSortedIter[K, V any](sorted []rs.Tuple2[K, V]) BTreeMap[K, V] {
	nodes := make([]*btreeNode[K, V], 0, len(sorted))
	for index := range sorted {
		if index+1 < len(sorted) && rs.Eq(sorted[index].F0, sorted[index+1].F0) {
			continue
		}
		nodes = append(nodes, &btreeNode[K, V]{
			key: sorted[index].F0, value: sorted[index].F1,
			left: nil, right: nil, parent: nil, height: 1,
		})
	}
	return BTreeMap[K, V]{root: btreeBuild(nodes, nil), length: len(nodes), rooted: true}
}

// Clone is BTreeMap::clone: a map with a copy of every entry, keys and values
// cloned by rs.Clone in key order.
func (m BTreeMap[K, V]) Clone() BTreeMap[K, V] {
	return m.cloneWith(rs.Clone[K], rs.Clone[V])
}

func (m BTreeMap[K, V]) cloneWith(cloneKey func(K) K, cloneValue func(V) V) BTreeMap[K, V] {
	if m.length == 0 {
		return BTreeMapNew[K, V]()
	}
	return BTreeMap[K, V]{
		root:   btreeCloneSubtree(m.root, nil, cloneKey, cloneValue),
		length: m.length,
		rooted: true,
	}
}

// Clear is BTreeMap::clear: it removes every entry.
func (m *BTreeMap[K, V]) Clear() {
	*m = BTreeMapNew[K, V]()
}

// Get is BTreeMap::get: a pointer to the value stored for key, and whether
// there is one. The pointer is nil when the key is absent.
func (m BTreeMap[K, V]) Get(key K) (*V, bool) {
	if n := m.find(key); n != nil {
		return &n.value, true
	}
	return nil, false
}

// GetKeyValue is BTreeMap::get_key_value: the stored key and a pointer to
// its value.
func (m BTreeMap[K, V]) GetKeyValue(key K) (K, *V, bool) {
	if n := m.find(key); n != nil {
		return n.key, &n.value, true
	}
	var zero K
	return zero, nil, false
}

// FirstKeyValue is BTreeMap::first_key_value: the entry with the smallest
// key.
func (m BTreeMap[K, V]) FirstKeyValue() (K, *V, bool) {
	if n := m.firstNode(); n != nil {
		return n.key, &n.value, true
	}
	var zero K
	return zero, nil, false
}

// FirstEntry is BTreeMap::first_entry: the entry with the smallest key, for
// changing or removing in place.
func (m *BTreeMap[K, V]) FirstEntry() rs.Option[BTreeMapOccupiedEntry[K, V]] {
	if n := m.firstNode(); n != nil {
		return rs.Some(BTreeMapOccupiedEntry[K, V]{m: m, node: n})
	}
	return rs.None[BTreeMapOccupiedEntry[K, V]]()
}

// PopFirst is BTreeMap::pop_first: it removes and returns the entry with the
// smallest key.
func (m *BTreeMap[K, V]) PopFirst() rs.Option[rs.Tuple2[K, V]] {
	if n := m.firstNode(); n != nil {
		m.unlink(n)
		return rs.Some(rs.Tuple2[K, V]{F0: n.key, F1: n.value})
	}
	return rs.None[rs.Tuple2[K, V]]()
}

// LastKeyValue is BTreeMap::last_key_value: the entry with the greatest key.
func (m BTreeMap[K, V]) LastKeyValue() (K, *V, bool) {
	if n := m.lastNode(); n != nil {
		return n.key, &n.value, true
	}
	var zero K
	return zero, nil, false
}

// LastEntry is BTreeMap::last_entry: the entry with the greatest key, for
// changing or removing in place.
func (m *BTreeMap[K, V]) LastEntry() rs.Option[BTreeMapOccupiedEntry[K, V]] {
	if n := m.lastNode(); n != nil {
		return rs.Some(BTreeMapOccupiedEntry[K, V]{m: m, node: n})
	}
	return rs.None[BTreeMapOccupiedEntry[K, V]]()
}

// PopLast is BTreeMap::pop_last: it removes and returns the entry with the
// greatest key.
func (m *BTreeMap[K, V]) PopLast() rs.Option[rs.Tuple2[K, V]] {
	if n := m.lastNode(); n != nil {
		m.unlink(n)
		return rs.Some(rs.Tuple2[K, V]{F0: n.key, F1: n.value})
	}
	return rs.None[rs.Tuple2[K, V]]()
}

// ContainsKey is BTreeMap::contains_key.
func (m BTreeMap[K, V]) ContainsKey(key K) bool {
	return m.find(key) != nil
}

// GetMut is BTreeMap::get_mut: a pointer to the value stored for key, and
// whether there is one. It is the same pointer Get returns.
func (m *BTreeMap[K, V]) GetMut(key K) (*V, bool) {
	if n := m.find(key); n != nil {
		return &n.value, true
	}
	return nil, false
}

// Insert is BTreeMap::insert: it stores value under key and returns the value
// the key had. When the key is already present the stored key is kept, which
// matters for keys that compare equal without being identical.
func (m *BTreeMap[K, V]) Insert(key K, value V) rs.Option[V] {
	found, parent, right := m.locate(key)
	if found != nil {
		old := found.value
		found.value = value
		return rs.Some(old)
	}
	m.link(&btreeNode[K, V]{key: key, value: value, left: nil, right: nil, parent: nil, height: 1}, parent, right)
	return rs.None[V]()
}

// Remove is BTreeMap::remove: it removes key and returns the value it had.
func (m *BTreeMap[K, V]) Remove(key K) rs.Option[V] {
	if n := m.find(key); n != nil {
		m.unlink(n)
		return rs.Some(n.value)
	}
	return rs.None[V]()
}

// RemoveEntry is BTreeMap::remove_entry: it removes key and returns the
// stored key with its value.
func (m *BTreeMap[K, V]) RemoveEntry(key K) rs.Option[rs.Tuple2[K, V]] {
	if n := m.find(key); n != nil {
		m.unlink(n)
		return rs.Some(rs.Tuple2[K, V]{F0: n.key, F1: n.value})
	}
	return rs.None[rs.Tuple2[K, V]]()
}

// Retain is BTreeMap::retain: it keeps the entries for which f returns true,
// visiting them in key order. f may change the value through the pointer.
func (m *BTreeMap[K, V]) Retain(f func(K, *V) bool) {
	for n := m.firstNode(); n != nil; {
		if n.height == 0 {
			rs.PanicAt(1, btreeIterationMessage)
		}
		following := n.next()
		if !f(n.key, &n.value) {
			m.unlink(n)
		}
		n = following
	}
}

// Append is BTreeMap::append: it moves every entry of other into m and
// leaves other empty. For a key present in both, m keeps its own key and
// takes other's value.
func (m *BTreeMap[K, V]) Append(other *BTreeMap[K, V]) {
	taken := *other
	*other = BTreeMapNew[K, V]()

	// Do we have to append anything at all?
	if taken.length == 0 {
		return
	}

	// We can just swap `self` and `other` if `self` is empty.
	if m.length == 0 {
		*m = taken
		return
	}

	incoming := taken.nodes()
	if len(incoming)*bits.Len(uint(m.length)) < m.length {
		// Few entries arrive: put each in its place.
		for _, node := range incoming {
			found, parent, right := m.locate(node.key)
			if found != nil {
				found.value = node.value
				node.left, node.right, node.parent, node.height = nil, nil, nil, 0
				continue
			}
			m.link(node, parent, right)
		}
		return
	}

	// Merge the two key-ordered sequences and build the tree again.
	own := m.nodes()
	merged := make([]*btreeNode[K, V], 0, len(own)+len(incoming))
	i, j := 0, 0
	for i < len(own) && j < len(incoming) {
		order := rs.Cmp(own[i].key, incoming[j].key)
		switch {
		case order < 0:
			merged = append(merged, own[i])
			i++
		case order > 0:
			merged = append(merged, incoming[j])
			j++
		default:
			own[i].value = incoming[j].value
			incoming[j].left, incoming[j].right, incoming[j].parent, incoming[j].height = nil, nil, nil, 0
			merged = append(merged, own[i])
			i++
			j++
		}
	}
	merged = append(merged, own[i:]...)
	merged = append(merged, incoming[j:]...)
	m.root = btreeBuild(merged, nil)
	m.length = len(merged)
}

// Range is BTreeMap::range: the entries whose keys lie between the two
// bounds, in key order (see Bound for how a Rust range is written). Like
// Rust it checks the bounds when it is called, and panics when the start is
// greater than the end or when both are excluded and equal.
func (m BTreeMap[K, V]) Range(lo, hi Bound[K]) iter.Seq2[K, *V] {
	first, last, failure := m.rangeSearch(lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(K, *V) bool) {
		btreeAscend(first, last, func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
	}
}

// RangeRev is `map.range(..).rev()`: the entries of Range from the greatest
// key down.
func (m BTreeMap[K, V]) RangeRev(lo, hi Bound[K]) iter.Seq2[K, *V] {
	first, last, failure := m.rangeSearch(lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(K, *V) bool) {
		btreeDescend(first, last, func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
	}
}

// RangeMut is BTreeMap::range_mut. The values of Range are already
// changeable through their pointers; this is the same sequence.
func (m *BTreeMap[K, V]) RangeMut(lo, hi Bound[K]) iter.Seq2[K, *V] {
	first, last, failure := m.rangeSearch(lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(K, *V) bool) {
		btreeAscend(first, last, func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
	}
}

// RangeMutRev is `map.range_mut(..).rev()`.
func (m *BTreeMap[K, V]) RangeMutRev(lo, hi Bound[K]) iter.Seq2[K, *V] {
	first, last, failure := m.rangeSearch(lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(K, *V) bool) {
		btreeDescend(first, last, func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
	}
}

// rangeSearch is range_search in navigate.rs with the check of
// search_tree_for_bifurcation in search.rs: it returns the first and the
// last node of the range (both nil when the range holds nothing), or the
// message Rust panics with for bounds that make no range.
func (m BTreeMap[K, V]) rangeSearch(lo, hi Bound[K]) (first, last *btreeNode[K, V], failure string) {
	loKind, loKey := unpackBound(lo)
	hiKind, hiKey := unpackBound(hi)
	if !m.rooted {
		return nil, nil, ""
	}
	if loKind != boundUnbounded && hiKind != boundUnbounded {
		if loKind == boundExcluded && hiKind == boundExcluded && rs.Eq(loKey, hiKey) {
			return nil, nil, "range start and end are equal and excluded in " + m.typeName()
		}
		if rs.Cmp(loKey, hiKey) > 0 {
			return nil, nil, "range start is greater than range end in " + m.typeName()
		}
	}
	first = m.lowerBound(loKind, loKey)
	if first == nil {
		return nil, nil, ""
	}
	last = m.upperBound(hiKind, hiKey)
	if last == nil {
		return nil, nil, ""
	}
	// Both ends exist, but no key may lie between the bounds, and then the
	// first node at or after the start comes after the last one at or before
	// the end.
	if first != last && rs.Cmp(first.key, last.key) > 0 {
		return nil, nil, ""
	}
	return first, last, ""
}

// typeName tells a map from the map inside a set, as IsSetVal in set_val.rs
// does for the text of a panic.
func (m BTreeMap[K, V]) typeName() string {
	var value V
	if _, isSet := any(value).(btreeSetVal); isSet {
		return "BTreeSet"
	}
	return "BTreeMap"
}

// Entry is BTreeMap::entry: the place of key in the map, for inserting or
// changing in one lookup. The entry must be used before the map is changed
// in any other way.
//
//	*m.Entry(key).OrDefault() += 1
//
//	switch entry := m.Entry(key).(type) {
//	case *collections.BTreeMapEntry_Occupied[string, int]:
//		entry.F0.Remove()
//	case *collections.BTreeMapEntry_Vacant[string, int]:
//		entry.F0.Insert(1)
//	default:
//		panic(rs.Unreachable(entry))
//	}
func (m *BTreeMap[K, V]) Entry(key K) BTreeMapEntry[K, V] {
	found, parent, right := m.locate(key)
	if found != nil {
		return &BTreeMapEntry_Occupied[K, V]{F0: BTreeMapOccupiedEntry[K, V]{m: m, node: found}}
	}
	return &BTreeMapEntry_Vacant[K, V]{F0: BTreeMapVacantEntry[K, V]{m: m, key: key, parent: parent, right: right}}
}

// SplitOff is BTreeMap::split_off: it moves every entry whose key is at or
// after key into a new map, which it returns. It takes time in proportion to
// the size of the map.
func (m *BTreeMap[K, V]) SplitOff(key K) BTreeMap[K, V] {
	if m.length == 0 {
		return BTreeMapNew[K, V]()
	}
	nodes := m.nodes()
	at := sort.Search(len(nodes), func(index int) bool { return rs.Cmp(key, nodes[index].key) <= 0 })
	left, right := nodes[:at], nodes[at:]
	m.root = btreeBuild(left, nil)
	m.length = len(left)
	return BTreeMap[K, V]{root: btreeBuild(right, nil), length: len(right), rooted: true}
}

// ExtractIf is BTreeMap::extract_if: a sequence that visits the entries
// between the two bounds in key order and removes and yields those for which
// pred returns true. It is lazy as in Rust: entries are removed as the
// sequence is consumed, and an entry not reached stays in the map. Unlike
// Range it does not check that the bounds make a range.
func (m *BTreeMap[K, V]) ExtractIf(lo, hi Bound[K], pred func(K, *V) bool) iter.Seq2[K, V] {
	loKind, loKey := unpackBound(lo)
	hiKind, hiKey := unpackBound(hi)
	return func(yield func(K, V) bool) {
		for n := m.lowerBound(loKind, loKey); n != nil; {
			if n.height == 0 {
				rs.PanicAt(1, btreeIterationMessage)
			}
			// On creation, we navigated directly to the left bound, so we
			// need only check the right bound here to decide whether to stop.
			switch hiKind {
			case boundIncluded:
				if rs.Cmp(n.key, hiKey) > 0 {
					return
				}
			case boundExcluded:
				if rs.Cmp(n.key, hiKey) >= 0 {
					return
				}
			case boundUnbounded:
			}
			following := n.next()
			if pred(n.key, &n.value) {
				m.unlink(n)
				if !yield(n.key, n.value) {
					return
				}
			}
			n = following
		}
	}
}

// IntoKeys is BTreeMap::into_keys: the keys in order, for a map that is not
// used again.
func (m BTreeMap[K, V]) IntoKeys() iter.Seq[K] {
	return m.Keys()
}

// IntoKeysRev is `map.into_keys().rev()`.
func (m BTreeMap[K, V]) IntoKeysRev() iter.Seq[K] {
	return m.KeysRev()
}

// IntoValues is BTreeMap::into_values: the values in key order, for a map
// that is not used again.
func (m BTreeMap[K, V]) IntoValues() iter.Seq[V] {
	return func(yield func(V) bool) {
		btreeAscend(m.firstNode(), nil, func(n *btreeNode[K, V]) bool { return yield(n.value) })
	}
}

// IntoValuesRev is `map.into_values().rev()`.
func (m BTreeMap[K, V]) IntoValuesRev() iter.Seq[V] {
	return func(yield func(V) bool) {
		btreeDescend(nil, m.lastNode(), func(n *btreeNode[K, V]) bool { return yield(n.value) })
	}
}

// IntoIter is BTreeMap::into_iter (`for (k, v) in map`): the entries in key
// order with their values moved out, for a map that is not used again.
func (m BTreeMap[K, V]) IntoIter() iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		btreeAscend(m.firstNode(), nil, func(n *btreeNode[K, V]) bool { return yield(n.key, n.value) })
	}
}

// IntoIterRev is `map.into_iter().rev()`.
func (m BTreeMap[K, V]) IntoIterRev() iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		btreeDescend(nil, m.lastNode(), func(n *btreeNode[K, V]) bool { return yield(n.key, n.value) })
	}
}

// Extend is BTreeMap::extend: it inserts every pair of seq, as Insert does.
func (m *BTreeMap[K, V]) Extend(seq iter.Seq2[K, V]) {
	for key, value := range seq {
		m.Insert(key, value)
	}
}

// Equal is BTreeMap's PartialEq: the same number of entries, and equal keys
// and values (by rs.Eq) entry for entry.
func (m BTreeMap[K, V]) Equal(other *BTreeMap[K, V]) bool {
	if m.length != other.length {
		return false
	}
	a, b := m.firstNode(), other.firstNode()
	for a != nil && b != nil {
		if !rs.Eq(a.key, b.key) || !rs.Eq(a.value, b.value) {
			return false
		}
		a, b = a.next(), b.next()
	}
	return true
}

// Compare is BTreeMap's Ord: the entries compared in order as (key, value)
// pairs by rs.Cmp, and the shorter map first when one is a prefix of the
// other.
func (m BTreeMap[K, V]) Compare(other *BTreeMap[K, V]) int {
	a, b := m.firstNode(), other.firstNode()
	for {
		switch {
		case a == nil && b == nil:
			return 0
		case a == nil:
			return -1
		case b == nil:
			return 1
		}
		if order := rs.Cmp(a.key, b.key); order != 0 {
			return order
		}
		if order := rs.Cmp(a.value, b.value); order != 0 {
			return order
		}
		a, b = a.next(), b.next()
	}
}

// Index is `map[&key]`: a pointer to the value stored for key. It panics as
// Rust does when the key is absent.
func (m BTreeMap[K, V]) Index(key K) *V {
	n := m.find(key)
	if n == nil {
		rs.PanicAt(1, "no entry found for key")
	}
	return &n.value
}

// Iter is BTreeMap::iter (`for (k, v) in &map`): every entry in key order,
// as the key and a pointer to the value in the map.
func (m BTreeMap[K, V]) Iter() iter.Seq2[K, *V] {
	return func(yield func(K, *V) bool) {
		btreeAscend(m.firstNode(), nil, func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
	}
}

// IterRev is `map.iter().rev()`: every entry from the greatest key down.
func (m BTreeMap[K, V]) IterRev() iter.Seq2[K, *V] {
	return func(yield func(K, *V) bool) {
		btreeDescend(nil, m.lastNode(), func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
	}
}

// IterMut is BTreeMap::iter_mut (`for (k, v) in &mut map`). The values of
// Iter are already changeable through their pointers; this is the same
// sequence.
func (m *BTreeMap[K, V]) IterMut() iter.Seq2[K, *V] {
	return BTreeMap[K, V].Iter(*m)
}

// IterMutRev is `map.iter_mut().rev()`.
func (m *BTreeMap[K, V]) IterMutRev() iter.Seq2[K, *V] {
	return BTreeMap[K, V].IterRev(*m)
}

// Keys is BTreeMap::keys: the keys in order.
func (m BTreeMap[K, V]) Keys() iter.Seq[K] {
	return func(yield func(K) bool) {
		btreeAscend(m.firstNode(), nil, func(n *btreeNode[K, V]) bool { return yield(n.key) })
	}
}

// KeysRev is `map.keys().rev()`.
func (m BTreeMap[K, V]) KeysRev() iter.Seq[K] {
	return func(yield func(K) bool) {
		btreeDescend(nil, m.lastNode(), func(n *btreeNode[K, V]) bool { return yield(n.key) })
	}
}

// Values is BTreeMap::values: a pointer to each value, in key order.
func (m BTreeMap[K, V]) Values() iter.Seq[*V] {
	return func(yield func(*V) bool) {
		btreeAscend(m.firstNode(), nil, func(n *btreeNode[K, V]) bool { return yield(&n.value) })
	}
}

// ValuesRev is `map.values().rev()`.
func (m BTreeMap[K, V]) ValuesRev() iter.Seq[*V] {
	return func(yield func(*V) bool) {
		btreeDescend(nil, m.lastNode(), func(n *btreeNode[K, V]) bool { return yield(&n.value) })
	}
}

// ValuesMut is BTreeMap::values_mut. It is the same sequence as Values.
func (m *BTreeMap[K, V]) ValuesMut() iter.Seq[*V] {
	return BTreeMap[K, V].Values(*m)
}

// ValuesMutRev is `map.values_mut().rev()`.
func (m *BTreeMap[K, V]) ValuesMutRev() iter.Seq[*V] {
	return BTreeMap[K, V].ValuesRev(*m)
}

// Len is BTreeMap::len: the number of entries.
func (m BTreeMap[K, V]) Len() int {
	return m.length
}

// IsEmpty is BTreeMap::is_empty.
func (m BTreeMap[K, V]) IsEmpty() bool {
	return m.length == 0
}

// The methods below describe the map to rs's formatter, structural clone and
// comparison, and to go/crates/serde (rs.ReflectMap).

// ReflectLen returns the number of entries.
func (m *BTreeMap[K, V]) ReflectLen() int { return m.length }

// ReflectKeyType returns the type K.
func (m *BTreeMap[K, V]) ReflectKeyType() reflect.Type { return reflect.TypeFor[K]() }

// ReflectValueType returns the type V.
func (m *BTreeMap[K, V]) ReflectValueType() reflect.Type { return reflect.TypeFor[V]() }

// ReflectEntries calls yield with each key and a pointer to its value, in
// key order.
func (m *BTreeMap[K, V]) ReflectEntries(yield func(key any, value any) bool) {
	btreeAscend(m.firstNode(), nil, func(n *btreeNode[K, V]) bool { return yield(n.key, &n.value) })
}

// ReflectInsert stores value (a V) under key (a K), as Insert does.
func (m *BTreeMap[K, V]) ReflectInsert(key any, value any) {
	m.Insert(reflectCast[K](key), reflectCast[V](value))
}

// ReflectSorted reports that iteration is in key order.
func (m *BTreeMap[K, V]) ReflectSorted() bool { return true }

var _ rs.ReflectMap = (*BTreeMap[int, int])(nil)

// reflectCast turns a value that arrived through one of the Reflect
// interfaces back into its static type. A nil interface stands for the zero
// value of a pointer or interface type.
func reflectCast[T any](value any) T {
	if value == nil {
		var zero T
		return zero
	}
	return value.(T)
}
