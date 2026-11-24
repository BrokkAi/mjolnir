// Port of brokk-sessionwiki 0.34.28 src/adapters/antigravity.rs.

package brokksessionwiki

// Antigravity reads Antigravity IDE or CLI conversations.
type Antigravity struct {
	unportedAdapter
}

// AntigravityCLIInHome is the Antigravity CLI conversations under one home.
func AntigravityCLIInHome(home string) *Antigravity {
	panic("unported: brokk-sessionwiki src/adapters/antigravity.rs Antigravity::cli_in_home")
}
