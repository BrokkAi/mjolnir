// Port of mj-daemon/src/pollers/quota.rs (only what a dashboard asks so
// far).

package daemon

// CompleteManualQuotaRefresh reports whether a refresh the person asked for
// is over. pendingCycles is the daemon's finished-cycle count at the moment
// of the request, and finishedCycles is its count now. A cycle already
// running when the request arrived can end first and complete the notice a
// few seconds early; the report itself is what the row shows, so that is
// harmless.
func CompleteManualQuotaRefresh(pendingCycles **uint64, finishedCycles uint64) bool {
	if *pendingCycles == nil || finishedCycles <= **pendingCycles {
		return false
	}
	*pendingCycles = nil
	return true
}
