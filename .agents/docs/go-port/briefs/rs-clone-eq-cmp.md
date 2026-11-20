# Structural clone, equality, ordering and shared ownership, inside `go/rs`

Files: `go/rs/clone.go`, `eq.go`, `cmp.go`, `default.go`, `arc.go`, `cell.go`,
`mem.go` and their `_test.go`, plus the placeholders `cloneValue`,
`equalValues`, `compareValues` in `go/rs/pending.go` (keep `compareOrdered` as
the fast path). For differential tests, add operations to a new oracle module
file `go/port/oracle/src/std_cmp.rs` and say so in the report (do not edit
`Cargo.toml` or `main.rs`; the coordinator registers it).

Other people add files to `go/rs` at the same time (the formatter in
`fmt_*.go`; numbers, sorting and slices). Do not touch their files; keep the
package compiling at every step. Do not change exported signatures in
`reflect_api.go`, `registry.go`, `containers.go` (you may add).

Follow Rust 1.99.1 (`core/src/{clone.rs,cmp.rs,cell.rs,mem/}`,
`alloc/src/sync.rs`):

1. `rs.Clone[T]`, `rs.ClonePtr`: deep copy by reflection with the semantics of
   `#[derive(Clone)]`. Strings, numbers, bools and data-free enums copy as
   they are. Slices, arrays, Go maps, `rs.Option`, `rs.Result` and tuples copy
   element-wise. A pointer is a `Box<T>` and is copied deeply, unless the
   struct field holding it is tagged `arc` (`rs:"name,arc"`, see
   `rs.FieldRustName`): then that pointer, or the pointers inside a slice,
   option or map held by the field, are shared, as cloning an `Arc` shares. An
   interface holding a pointer to an enum-variant struct is copied deeply.
   Collections implementing `rs.ReflectMap`/`rs.ReflectSeq` are cloned through
   those interfaces into a new zero value of the same type. A type with
   `Clone() T` (value or pointer receiver) is copied by that method. Unexported
   fields are copied too (Rust derives over private fields). Types that must
   not be copied by reflection (sync primitives, channels, funcs, `rs.Arc`,
   anything with the marker method `rsNoReflectClone()`) are copied as plain
   values or handles: document the rule and provide the marker interface.
2. `rs.Eq[T]`: `#[derive(PartialEq)]` by reflection: field-wise, pointers by
   pointee, enum values equal when the same variant with equal fields, a nil
   slice equal to an empty one, floats by `==`, options, tuples, collections
   through the reflect interfaces, Go maps by content, `Equal(other T) bool` or
   `Equal(other *T) bool` used where present, unexported fields included, funcs
   and channels by identity.
3. `rs.Cmp[T]`: `#[derive(Ord)]`: fields in declaration order, enums by
   variant index from the registry then fields, data-free enums by value,
   `None < Some`, slices lexicographically, strings by bytes, `false < true`,
   tuples, floats by `partial_cmp` with incomparable as equal (document),
   `Compare(other T) int` or `Compare(other *T) int` where present.
4. `rs.Default`: add `rs.RegisterDefault[T](func() T)` for types whose Rust
   `Default` differs from the Go zero value (a `#[default]` variant that is not
   the first, a hand-written impl), and make `rs.Default[T]()` return the
   registered value, else the zero value with nested registered defaults
   applied. The declarations generator already emits `rs.RegisterDefault`
   calls in that form.
5. `rs.Arc[T]` and `rs.Weak[T]`, for the places where Rust uses `Weak`,
   `Arc::strong_count`, `Arc::try_unwrap`, `Arc::get_mut` or `Arc::ptr_eq` (see
   `.agents/docs/go-port/counted-arcs.md`): `rs.ArcNew`, `Clone`, idempotent
   `Drop`, `Get`, `rs.ArcDowngrade`, `Weak.Upgrade() Option[Arc[T]]`,
   `Weak.Clone`, `Weak.Drop`, `rs.WeakNew`, `rs.ArcStrongCount`,
   `rs.ArcWeakCount`, `rs.ArcTryUnwrap`, `rs.ArcGetMut`, `rs.ArcPtrEq`,
   `rs.WeakPtrEq`, `rs.ArcMakeMut`. When the strong count reaches zero the
   value's `Drop()` runs if it has one (`rs.Dropper`). A handle that becomes
   unreachable without `Drop()` still releases its count (`runtime.AddCleanup`)
   and, with `MJ_RS_LEAK_CHECK=1`, is reported with its creation site through a
   hook tests can assert on. Safe for concurrent use.
6. `rs.Cell[T]`, `rs.RefCell[T]` (thin; no borrow tracking; say so),
   `rs.Take`, `rs.Replace`, `rs.Swap`.

Tests: thorough table tests, including unexported fields, nested enums,
`arc`-tagged sharing (pointer identity shared, Box pointers not), collections
through small fakes of the reflect interfaces, concurrency tests for `Arc`
(without `-race`, since CGO is disabled), and the leak-check hook.
