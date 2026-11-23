// Port of mj-tui/src/retire.rs.

// Frees replaced values away from the event loop.
//
// Replacing a session's transcript on the dashboard drops the previous one,
// and that can be thousands of strings and rendered lines. In Rust, freeing
// them is work the loop would do before it can handle the next key, so the
// loop hands the old value to one background thread instead.
//
// Go has no destructors: the garbage collector reclaims an unreachable value
// on its own background workers, never on the goroutine that let go of it.
// Letting go of the last reference is therefore already the hand-off Rust's
// retire thread provides, and retire only marks the places that rely on it.

package tui

// retire lets go of value. The collector frees it off the event loop; see
// the file comment.
func retire[T any](value T) {
	_ = value
}
