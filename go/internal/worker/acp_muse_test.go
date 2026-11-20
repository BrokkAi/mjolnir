//go:build unix

// Port of mj-worker/src/acp/muse_tests.rs.

package worker

import "testing"

// nativeMuseTurn exercises the actual client and native host without
// printing credentials or provider transcripts in diagnostics. It runs one
// prompt through the Muse adapter at adapter, resuming the native session
// resume when it is not nil, and returns the native session id and the
// reply.
func nativeMuseTurn(t *testing.T, adapter, muse, home, cwd string, resume *string, prompt string) (string, string) {
	t.Helper()
	panic("unported: mj-worker/src/acp/muse_tests.rs native_muse_turn")
}
