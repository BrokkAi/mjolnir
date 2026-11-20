// Port of pulldown-cmark 0.10.3 src/strings.rs

package pulldowncmark

// CowStr, the crate's copy-on-write string (borrowed from the input, boxed,
// or inlined in three words), is a Go `string` everywhere in this package: a
// Go string is already an immutable view that can share the input's bytes,
// so the three representations have nothing to stand for. Its methods map as
// follows: `as_ref()`, `deref()`, `to_string()`, `into_string()` and
// `clone()` are the string itself, `==` is `==`, and `CowStr::from(char)` is
// `string(rune)`.
//
// InlineStr and StringTooLongError exist only to build the Inlined
// representation and are not ported.
