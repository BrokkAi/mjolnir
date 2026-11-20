# Integer and float operations, sorting, slice and iterator helpers, in `go/rs`

Files: `go/rs/num*.go`, `sort*.go`, `slice*.go`, `iter*.go` and their
`_test.go`. Oracle modules `go/port/oracle/src/std_num.rs` (feature `std_num`)
and `std_sort.rs` (feature `std_sort`).

Other people add files to `go/rs` at the same time. Do not touch their files
or `pending.go`; keep the package compiling at every step. `rs.Cmp` orders
numbers, strings and bools, and orders structs, tuples and enums once the
clone/eq/cmp work lands (it is in `go/rs/cmp.go`): take comparison functions as
arguments for arbitrary types and offer forms constrained to `cmp.Ordered`.
`rs.FromBool` (`usize::from(bool)`) already exists in `tuple.go`.

First find out what Mjolnir uses (grep `mj-*/` and `voice-worker/`): integer
methods (`checked_`, `saturating_`, `wrapping_`, `overflowing_`, `abs_diff`,
`pow`, `div_ceil`, `next_multiple_of`, `rem_euclid`, `leading_zeros`,
`trailing_zeros`, `count_ones`, `is_power_of_two`, `next_power_of_two`, the
byte conversions, `clamp`, `signum`, `u128`/`i128`), float methods (`round`,
`floor`, `ceil`, `trunc`, `clamp`, `max`/`min` NaN rules, `to_bits`,
`is_finite`, `mul_add`, `powi`/`powf`, `total_cmp`), `as` casts between floats
and integers, and slice, `Vec` and iterator methods. Cover everything used,
with Rust 1.99.1's exact semantics (`core/src/num/`, `core/src/slice/`,
`core/src/iter/`, `alloc/src/vec/`, `alloc/src/slice.rs`).

1. Numbers: generic `rs.CheckedAdd/Sub/Mul/Div/Rem/Neg/Pow/Shl/Shr` returning
   `rs.Option[T]`, `rs.SaturatingAdd/Sub/Mul/Pow`, `rs.WrappingAdd/Sub/Mul/Neg`,
   `rs.OverflowingAdd/Sub/Mul`, `rs.AbsDiff`, `rs.DivCeil`,
   `rs.NextMultipleOf`, `rs.RemEuclid`, `rs.DivEuclid`, `rs.Clamp`, and
   whatever else is used. `usize` is Go `int` in this port: provide
   `rs.UsizeCheckedSub`, `UsizeSaturatingSub`, `UsizeCheckedAdd`,
   `UsizeSaturatingAdd`, `UsizeCheckedMul`, `UsizeSaturatingMul` treating `int`
   as an unsigned value in [0, `rs.UsizeMax`]. Casts: `rs.CastF64ToI64`,
   `...ToU64`, `...ToI32`, `...ToU32`, `...ToUsize`, `...ToU8`, `...ToU16` and
   the `F32` forms, with Rust's saturating, NaN-to-zero `as` semantics. Float
   helpers wherever Go differs from Rust.
2. Sorting: `rs.SortStable` family (`sort`, `sort_by`, `sort_by_key`,
   `sort_by_cached_key`). `rs.SortUnstable`, `SortUnstableBy`,
   `SortUnstableByKey`: an exact port of Rust 1.99.1's
   `core::slice::sort::unstable` (ipnsort, its quicksort with the same pivot
   selection and partitioning, heapsort fallback, the small-sort networks and
   thresholds), because the order it leaves equal elements in is observable.
   Also `select_nth_unstable*` if used, `binary_search`, `binary_search_by`,
   `binary_search_by_key`, `partition_point` with Rust's exact probing, and
   `rs.IsSorted`. Verify `SortUnstable` against the oracle on arrays of (key,
   tag) pairs with many ties and patterns (random, ascending, descending,
   sawtooth, all equal, lengths 0 to 5000 across the small-sort thresholds),
   comparing the tag order; the same for binary-search probes.
3. Slice and `Vec` helpers with Rust names for what Go lacks or does
   differently (`Retain`, `RetainMut`, `Dedup*`, `Drain`, `SplitOff`,
   `Truncate`, `Insert`, `Remove`, `SwapRemove`, `Extend`, `Resize`, `Rotate*`,
   `Chunks`, `ChunksExact`, `Windows`, `ChunkBy`, `SplitFirst/Last`,
   `First`/`Last`/`Get` as options, `Contains`, `StartsWith`/`EndsWith`,
   `Concat`, `Join`, `Repeat`, `Fill`, `Reverse`, `Position`, `Rposition`,
   `Find`, `FindMap`, `Any`, `All`, `Count`, `Sum`, `Product`, `Min`, `Max`,
   `MinBy`, `MaxBy`, `MinByKey` (the first minimum), `MaxByKey` (the last
   maximum), `Fold`, `Map`, `Filter`, `FilterMap`, `FlatMap`, `Zip`, `Unzip`,
   `Enumerate`, `Skip`/`Take`/`StepBy`, `SkipWhile`/`TakeWhile`, `Partition`,
   `Peekable`, `Last`, `Nth`, `Rev`, `Chain`, ...). Name them `rs.<Name>` over
   `[]T`; prefix `Slice` or `Iter` only where names collide. Wrap Go's `slices`
   and `iter` where they already behave exactly as Rust. Keep evaluation order
   and short-circuiting.

Tests: the std tests for what you cover (`coretests/tests/num/`,
`coretests/tests/slice.rs`, `alloctests/tests/slice.rs`,
`alloctests/tests/vec.rs`, `alloctests/tests/sort/`), and differential tests
against the oracle for the numeric helpers (random and edge operands for every
operation and width, the casts on NaN, infinities and out-of-range values) and
for sorting and searching as above.
