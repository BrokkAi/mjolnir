// Port of toml_edit 0.20.2 src/internal_string.rs

package tomledit

// InternalString is toml_edit::InternalString, the opaque string storage
// internal to toml_edit. It is a plain String in this build (the `perf`
// feature, which swaps in kstring, is off), so here it is a Go string.
type InternalString = string
