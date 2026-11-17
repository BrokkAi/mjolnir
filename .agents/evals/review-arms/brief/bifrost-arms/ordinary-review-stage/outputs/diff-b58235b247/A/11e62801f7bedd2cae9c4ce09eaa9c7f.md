- [P1] Follow Python's method resolution order for multiple inheritance — crates/bifrost-python/src/graph/resolver.rs:59-62
  For `class C(A, B)` where `A` inherits `X` and both `X` and `B` define the member, Python resolves `C().member` to `X.member` when the MRO is `C, A, X, B, object`. This breadth-first search instead stops at `B`, so both usage queries and inverted edges attribute the call to the wrong declaration. It also returns both declarations when two direct bases define the member, although Python selects the first in the MRO.

- [P2] Allow a local import to replace a parameter binding — crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1957-1959
  In `def f(Worker): from pkg import Worker; return Worker()`, the import replaces the parameter before the reference, but `has_runtime_callable_binding_at` always reports the parameter as a runtime binding. The new early return consequently reports no definition for a valid imported symbol instead of resolving `pkg.Worker`.

- [P2] Account for reassignment of a function-local importlib binding — crates/bifrost-python/src/graph/extractor.rs:542-543
  When a function executes `import importlib; importlib = fake; canonical = importlib.import_module("pkg.mod"); canonical.member()`, this branch accepts the original import without checking the intervening assignment. The usage scan therefore reports `canonical.member` as a reference to `pkg.mod.member` even though the call was made on `fake`, producing a false positive for the exact-match behavior introduced here.
