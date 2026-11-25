// Port of brokk-sessionwiki 0.34.28 src/adapters/codex.rs.

package brokksessionwiki

// Codex reads Codex rollouts, live or archived.
type Codex struct {
	unportedAdapter
}

// CodexInHome is the live rollouts under one Codex home.
func CodexInHome(home string) *Codex {
	panic("unported: brokk-sessionwiki src/adapters/codex.rs Codex::in_home")
}

// CodexArchivedIn is the archived rollouts under one Codex home.
func CodexArchivedIn(home string) *Codex {
	panic("unported: brokk-sessionwiki src/adapters/codex.rs Codex::archived_in")
}
