// Port of the test-only items of mj-worker-staging/src/worker_binary/store.rs
// (`#[cfg(all(test, unix))]`).

//go:build unix

package workerstaging

// ForgetCollections clears every store's collection claim, so a test's
// install collects as a fresh daemon's would.
func ForgetCollections() {
	collections.mu.Lock()
	defer collections.mu.Unlock()
	clear(collections.last)
}
