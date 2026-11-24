// Port of brokk-sessionwiki 0.34.28 src/adapters/grok.rs.

package brokksessionwiki

// GrokBuild reads Grok Build sessions.
type GrokBuild struct {
	unportedAdapter
}

// GrokBuildInHome is the sessions under one Grok Build home.
func GrokBuildInHome(home string) *GrokBuild {
	panic("unported: brokk-sessionwiki src/adapters/grok.rs GrokBuild::in_home")
}

// KeepConfiguredPaths keys sessions by the configured home spelling,
// including symlinks, instead of the canonical path.
func (a *GrokBuild) KeepConfiguredPaths() *GrokBuild {
	panic("unported: brokk-sessionwiki src/adapters/grok.rs GrokBuild::keep_configured_paths")
}
