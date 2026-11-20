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
package rs
