# `go/rs/collections`: Rust's `std::collections`

Package `go/rs/collections`. Oracle module
`go/port/oracle/src/std_collections.rs`, feature `std_collections`.

The Rust code uses `BTreeMap` about 2,400 times and `BTreeSet` about 900 (key
order is part of every wire format and golden file), `HashMap`/`HashSet`
about 200, `VecDeque` about 100. Product code is translated call for call onto
these types, so they need Rust's API and behaviour (Rust 1.99.1,
`alloc/src/collections/`, `std/src/collections/`, tests beside them and in
`alloctests/tests/collections/`).

Build `BTreeMap[K, V]`, `BTreeSet[T]`, `HashMap[K, V]`, `HashSet[T]`,
`VecDeque[T]`:

- The zero value of each is an empty, usable collection. They are used through
  pointers; a value copy aliases storage (document it; `rs.Clone` copies
  through the reflect interfaces).
- B-tree ordering is `rs.Cmp`, for every comparison; no ordering of your own.
  Hash collections are over Go maps for comparable keys; check how the Rust
  code keys its hash maps and report any key type that is not comparable in Go.
- O(log n) operations for the B-tree types (a real balanced tree or B-tree).
- `Get(k) (*V, bool)` and `GetMut(k) (*V, bool)` return a pointer into the
  collection, valid until the next insertion or removal; store values so that
  pointers survive rebalancing where practical, and state the guarantee.
- The full Rust API in PascalCase: constructors (`BTreeMapNew`,
  `BTreeMapFrom`, `FromIter`), `Insert` (old value as an option), `Remove`,
  `RemoveEntry`, `ContainsKey`, `Len`, `IsEmpty`, `Clear`, `Entry` with
  `OrInsert`, `OrInsertWith`, `OrDefault`, `AndModify`, `Key` and occupied and
  vacant inspection, `FirstKeyValue`, `LastKeyValue`, `PopFirst`, `PopLast`,
  `Range` with inclusive, exclusive and unbounded bounds, `RangeMut`,
  `Retain`, `ExtractIf` if used, `Append`, `SplitOff`, `Extend`, `Keys`,
  `Values`, `ValuesMut`, `Iter`, `IterMut`, reverse iteration, `IntoKeys`,
  `IntoValues`; set algebra for sets; the whole `VecDeque` surface used. Grep
  the Rust sources to make sure nothing used is missing.
- Iteration with range-over-func: `for k, v := range m.Iter()` yields
  `(K, *V)` in key order for B-tree types. `HashMap`/`HashSet` iterate in Go
  order and also offer `IterSorted()`.
- Implement `rs.ReflectMap`/`rs.ReflectSeq` (`go/rs/containers.go`) on the
  pointer types.
- Rust's panic text where the Rust API panics.

Tests: the std tests for what you cover (the B-tree map and set suites), and
differential tests against the oracle: seeded random operation sequences on
both sides comparing the full contents and every return value after each step,
for integer and string keys.
