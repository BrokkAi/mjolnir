// Port of mj-worker/src/relay/journal.rs (its #[cfg(test)] items).

package worker

func (r *DurableRelay) retainedEventBodyCount() int {
	return len(r.hotEvents)
}

// setSealByteLimitOverride sets (or, with nil, clears) the active-segment seal
// threshold. Rust scopes it to the current test thread; see
// sealByteLimitOverride.
func setSealByteLimitOverride(limit *uint64) {
	sealByteLimitOverride.Store(limit)
}
