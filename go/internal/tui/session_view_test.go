// Port of mj-tui/src/session_view.rs (test-only items).

package tui

// cpuRollupDerivations is how many times the CPU rollups have been derived,
// without deriving them.
func (d *DashboardState) cpuRollupDerivations() int {
	return d.cpuRollupsField.derivations
}

// sessionViewDerivations is how many times the session facts and the
// Sessions order have been derived, without deriving them.
func (d *DashboardState) sessionViewDerivations() (int, int) {
	return d.sessionFactsField.derivations, d.sessionOrderField.derivations
}
