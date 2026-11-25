// Port of brokk-sessionwiki 0.34.28 src/adapters/codex/history.rs.

package brokksessionwiki

// CodexHistory reads threads Codex keeps only in its history database.
type CodexHistory struct {
	unportedAdapter
}

// CodexHistoryInHome is the history database under one Codex home.
func CodexHistoryInHome(home string) *CodexHistory {
	panic("unported: brokk-sessionwiki src/adapters/codex/history.rs CodexHistory::in_home")
}
