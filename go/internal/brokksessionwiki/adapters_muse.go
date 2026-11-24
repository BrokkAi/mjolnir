// Port of brokk-sessionwiki 0.34.28 src/adapters/muse.rs.

package brokksessionwiki

// Muse reads Muse sessions.
type Muse struct {
	unportedAdapter
}

// MuseInHome is the sessions under one Muse configuration home, in the data
// directory Muse derives from it.
func MuseInHome(home string) *Muse {
	panic("unported: brokk-sessionwiki src/adapters/muse.rs Muse::in_home")
}

// MuseInSessionsRoot is the sessions under one Muse sessions root.
func MuseInSessionsRoot(root string) *Muse {
	panic("unported: brokk-sessionwiki src/adapters/muse.rs Muse::in_sessions_root")
}

// KeepConfiguredPaths keys sessions by the configured root spelling,
// including symlinks, instead of the canonical path.
func (a *Muse) KeepConfiguredPaths() *Muse {
	panic("unported: brokk-sessionwiki src/adapters/muse.rs Muse::keep_configured_paths")
}
