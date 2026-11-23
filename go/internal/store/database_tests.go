// Port of the read hooks in mj-store/src/database/tests.rs.
//
// Rust compiles these hooks, and the calls to them in state_io.rs and
// materialized.rs, only into its test build, with the hook in a thread-local.
// Go cannot call test files from the package's other files, so the hooks
// live here. Only tests set them; otherwise each call is one atomic load.

package store

import "sync/atomic"

// afterStateSessionsReadHook runs once, after the state reader has read the
// sessions and before it reads their subagent relations, so a test can commit
// a concurrent change between the two queries (Rust's thread-local
// AFTER_STATE_SESSIONS_READ). Taking it clears it.
var afterStateSessionsReadHook atomic.Pointer[func()]

// afterStateSessionsRead runs and clears afterStateSessionsReadHook.
func afterStateSessionsRead() {
	if hook := afterStateSessionsReadHook.Swap(nil); hook != nil {
		(*hook)()
	}
}

// afterMaterializedFrontierReadHook runs once, after a projection reader has
// read the session's frontier and before it reads the transcript (Rust's
// thread-local AFTER_MATERIALIZED_FRONTIER_READ). Taking it clears it.
var afterMaterializedFrontierReadHook atomic.Pointer[func()]

// afterMaterializedFrontierRead runs and clears
// afterMaterializedFrontierReadHook.
func afterMaterializedFrontierRead() {
	if hook := afterMaterializedFrontierReadHook.Swap(nil); hook != nil {
		(*hook)()
	}
}
