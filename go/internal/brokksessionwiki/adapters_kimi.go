// Port of brokk-sessionwiki 0.34.28 src/adapters/kimi.rs.

package brokksessionwiki

// KimiCode reads Kimi Code sessions.
type KimiCode struct {
	unportedAdapter
}

// KimiCodeInHome is the sessions under one Kimi Code home.
func KimiCodeInHome(home string) *KimiCode {
	panic("unported: brokk-sessionwiki src/adapters/kimi.rs KimiCode::in_home")
}
