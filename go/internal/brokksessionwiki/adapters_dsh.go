// Port of brokk-sessionwiki 0.34.28 src/adapters/dsh.rs.

package brokksessionwiki

// Dsh reads DSH histories.
type Dsh struct {
	unportedAdapter
}

// DshInHome is the histories under one DSH home.
func DshInHome(home string) *Dsh {
	panic("unported: brokk-sessionwiki src/adapters/dsh.rs Dsh::in_home")
}
