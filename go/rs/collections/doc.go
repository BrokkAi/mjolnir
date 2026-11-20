// Package collections stands in for Rust's std::collections (Rust 1.99.1):
// alloc's BTreeMap, BTreeSet and VecDeque, std's HashMap, HashSet and
// DefaultHasher, and core::ops::Bound, which std::collections re-exports.
// Product code is translated onto it call for call
// (.agents/docs/go-port/translation-rules.md, rules 3, 4 and 7).
//
// # Types and names
//
//	BTreeMap<K, V>     BTreeMap[K, V]      BTreeMapNew, BTreeMapFrom, BTreeMapFromIter
//	BTreeSet<T>        BTreeSet[T]         BTreeSetNew, BTreeSetFrom, BTreeSetFromIter
//	HashMap<K, V>      HashMap[K, V]       HashMapNew, HashMapWithCapacity, HashMapFrom, HashMapFromIter
//	HashSet<T>         HashSet[T]          HashSetNew, HashSetWithCapacity, HashSetFrom, HashSetFromIter
//	VecDeque<T>        VecDeque[T]         VecDequeNew, VecDequeWithCapacity, VecDequeFrom, VecDequeFromIter
//	Vec::from(deque)   VecFromVecDeque
//	btree_map::Entry   BTreeMapEntry (and BTreeMapOccupiedEntry, BTreeMapVacantEntry)
//	hash_map::Entry    HashMapEntry (and HashMapOccupiedEntry, HashMapVacantEntry)
//	Bound<T>           Bound[T]            Included, Excluded, Unbounded
//	DefaultHasher      HashMapDefaultHasher
//
// Methods keep their Rust names in PascalCase. A method that takes a Rust
// range takes its two bounds (see Bound). A lookup that returns references in
// Rust returns a pointer and a found flag (`v, ok := m.Get(k)`); a method
// that moves a value out returns an rs.Option. `.rev()` on an iterator is the
// method's Rev form (IterRev, KeysRev, RangeRev, DrainRev, ...). `map[&k]`
// and `deque[i]` are Index (and IndexMut); `==` and `cmp` are Equal and
// Compare, which rs.Eq and rs.Cmp call; `a | b`, `a & b`, `a ^ b` and `a - b`
// on sets are Bitor, Bitand, Bitxor and Sub. Generic methods that Go cannot
// express as methods are functions: VecDequeBinarySearchByKey.
//
// # Values, copies and pointers
//
// The zero value of every collection is an empty, usable collection, the
// same as its New. A collection is changed through a pointer to it. Assigning
// a collection to another variable does not copy its contents: both values
// then share the storage, which is what a Rust move leaves behind (the
// source is not used again). The pointer types implement rs.ReflectMap
// (BTreeMap, HashMap) and rs.ReflectSeq (BTreeSet, HashSet, VecDeque),
// through which the formatter, rs.Clone, rs.Eq, rs.Cmp and serde reach them.
// `x.clone()` is rs.Clone(x), which rebuilds the collection through those
// interfaces, cloning keys, values and elements as their own types say; a
// pointer that stands for an Arc, under an `arc` tag on the field holding the
// collection, is shared and not copied, as Rust's clone shares an Arc. The
// collections have no Clone method on purpose: rs.Clone would call it in
// place of the rebuild, and it could not see the tag. A cloned VecDeque has
// the same elements in a buffer grown by pushes, where Rust's clone
// allocates exactly the length.
//
// Get, GetMut, the entry methods, the first and last accessors and the
// iterators of the maps hand out a *V that points at the value stored in the
// map. Each map entry lives in its own heap cell that is never moved or
// reused, so such a pointer stays valid, and keeps pointing at that entry's
// value, for as long as the entry is in the map: inserting or removing other
// entries, Retain, Append and SplitOff do not disturb it. Once the entry is
// removed the pointer still reads the value it had, but is no longer
// connected to the map. Rust guarantees much less (every reference ends at
// the next change), so code translated from Rust never relies on more. A
// VecDeque stores its elements inline, as Rust does: its pointers are valid
// until the next call that adds, removes or moves elements.
//
// # Order and iteration
//
// BTreeMap and BTreeSet order keys by rs.Cmp and by nothing else, so a
// derived Ord, a hand-written Compare method, numbers and strings (byte by
// byte) order exactly as in Rust. Paths are the exception to watch: Rust
// orders and compares a PathBuf by its components ("a/b" before "a-b", and
// "a//b" equal to "a/b"), and a path held as a Go string orders by bytes. A
// collection keyed by paths needs a key type whose Compare and HashKey call
// rspath.Cmp and rspath.HashKey. Iteration is a Go range-over-func
// sequence, in key order: `for k, v := range m.Iter()` yields each key and a
// pointer to its value. HashMap and HashSet iterate in Go's map order, which
// changes from one loop to the next as Rust's does from one run to the next;
// IterSorted gives the same entries in key order where output must be
// deterministic. Changing the set of entries of a collection while it is
// being iterated is not supported, as in Rust, except that a map or set
// loop may remove the entry it was just given; an iteration of a B-tree
// collection that finds its next entry gone panics rather than stop early.
// Sequences are lazy, like Rust's iterators, but a sequence ranged a second
// time starts over where a Rust iterator would resume.
//
// # Hash keys
//
// HashMap and HashSet take a comparable key type and decide once per key
// type how keys are told apart, so that two keys are one key exactly when
// Rust's Eq says so: by HashKey for a type with a hand-written Hash (a
// `HashKey() string` method, translation rule 2); by Go's == for strings,
// numbers, bools, data-free enums and arrays and structs of those; and by
// rs.Cmp for everything else that holds a pointer or an interface (an enum
// whose variants carry data, a Box or Arc, an Option of one, or a struct
// holding them), which Go's == would compare by address. Such keys need no
// method, as a derived Hash needs none, and cost a logarithmic search
// instead of a hash lookup. A key type that Go cannot compare at all, a
// slice for Rust's Vec, cannot be a key: Mjolnir has one such map,
// `HashMap<Vec<u32>, u32>` in mj-transcript/src/github_history.rs.
//
// HashMapDefaultHasher is SipHash 1-3 with zero keys, the DefaultHasher that
// Mjolnir uses to derive stable hashes (mj-daemon's mbx_service.rs and
// mj-transcript's github_history.rs). Its output for the same writes is
// Rust's; the type's documentation says how `value.hash(&mut hasher)` is
// written for each kind of value.
//
// # Panics
//
// Where Rust panics the port panics with Rust's message, through rs.PanicAt
// so that the location is the caller's: a BTreeMap or BTreeSet range whose
// start is greater than its end, or whose ends are equal and both excluded
// (checked, as in Rust, only once the collection has had a root node, which
// an emptied collection keeps and a new or cleared one lacks); `map[&k]` of
// an absent key; VecDeque index, swap, insert, split_off, rotate and range
// checks, and capacity overflow.
//
// # VecDeque layout
//
// VecDeque follows Rust's ring buffer and growth policy step by step, so
// Capacity and the split AsSlices reports are the ones Rust gives after the
// same calls. Two things cannot be reproduced: Rust sizes the first
// allocation by the size of T as Rust lays it out (8 elements for a one-byte
// T, else 4, or 1 above 1 KiB), which here is Go's size of T; and Extend
// and VecDequeFromIter grow as Rust does for an iterator of unknown length,
// because a Go sequence has no size hint, where Rust reserves the exact
// length of a slice or Vec iterator in one step. Neither changes the
// contents. Mjolnir never reads a deque's capacity.
//
// # Not covered
//
//   - BinaryHeap and LinkedList, which Mjolnir does not use.
//   - The unstable APIs: B-tree cursors (lower_bound, upper_bound, ...),
//     BTreeMap::merge and try_insert, BTreeSet::entry and get_or_insert,
//     VecDeque::extract_if, splice, extend_front, prepend and
//     truncate_to_range, HashMap::try_insert, and allocators.
//   - HashMap and HashSet capacity, hasher parameters and their
//     constructors, try_reserve and get_disjoint_mut; VecDeque::try_reserve
//     and try_reserve_exact. A Go map has no capacity to report, and
//     Mjolnir calls none of them.
//   - Lookups through the Borrow trait: every lookup takes the key type.
//   - Rust's B-tree node layout. The B-tree collections are a balanced
//     binary tree (btree_node.go) with the same contract; nothing a caller
//     can observe depends on the nodes, except what a key type with an
//     inconsistent order (Ord not transitive) does, which Rust leaves
//     unspecified and which differs.
//   - Destructor behaviour (what is dropped when a predicate or a clone
//     panics): Go has no destructors.
//
// # Tests
//
// btree_map_test.go, btree_set_test.go and vec_deque_test.go are alloc's
// suites (src/collections/btree/map/tests.rs, set/tests.rs and
// vec_deque/tests.rs), each file listing the tests that have no port and
// why. std's HashMap and HashSet suites are not ported; the differential
// tests cover those types. The differential tests (oracle_cases_test.go,
// with the Go side of each operation in oracle_test.go) run seeded random
// scripts of operations on every collection, with integer and string keys
// (i64 and u8 elements for the deque), on both sides, comparing every
// return value and the full content (the deque's layout and capacity
// included) after every step, against the oracle module
// go/port/oracle/src/std_collections.rs. btree_check_test.go checks the
// port's own tree after every step of those scripts and of a model test,
// hash_keys_test.go checks how hash keys are told apart, and clone_test.go
// how rs.Clone treats the pointers inside a collection.
package collections
