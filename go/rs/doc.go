// Package rs gives Go code the parts of Rust's core library that Mjolnir's
// behaviour depends on: Option and Result with their panic text, Rust's
// formatting language, structural clone, equality and ordering, counted
// references, and panics that behave as Rust panics do.
//
// It exists so that the Go implementation of Mjolnir can be a translation of
// the Rust one, call for call. The rules for using it are in
// .agents/docs/go-port/translation-rules.md, and the contract of every package
// that stands in for a Rust library is in
// .agents/docs/go-port/runtime-packages.md.
//
// Stands in for: core and alloc of Rust 1.99.1 (option, result, fmt, cmp,
// clone, mem, num, char, cell, panic, any, iter, slice::sort, sync::Arc).
//
// Clone, equality, ordering and ownership (clone.go, eq.go, cmp.go,
// default.go, arc.go, cell.go, mem.go) follow core::clone, core::cmp,
// core::default, core::cell, core::mem and alloc::sync: what the derives
// generate, done by reflection (Clone, ClonePtr, Eq, Cmp, PartialCmp, OrdCmp,
// Min, Max, Default, RegisterDefault), Ordering and Reverse, Arc and Weak
// with Rust's counting (Rc and rc::Weak too), Cell, RefCell, Take, Replace,
// Swap, Forget and Discriminant. Each file's comments give its rules, and the
// tests compare them with Rust through the oracle module std_cmp. Not
// covered: RefCell's borrow tracking (a second mutable borrow succeeds),
// UnsafeCell, Arc::new_cyclic, into_raw and from_raw, cmp::min_by, max_by,
// min_by_key, max_by_key and minmax, which Mjolnir does not use. Cmp also
// orders what Rust cannot (a HashMap, a func), so as to be total.
package rs
