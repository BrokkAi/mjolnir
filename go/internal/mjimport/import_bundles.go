// Port of mj-import/src/import/bundles.rs.

package mjimport

import (
	"log/slog"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// SetupStyleID is Rust's re-export of the store's `setup_style_id`.
func SetupStyleID(value string) string {
	return store.SetupStyleID(value)
}

// RepositoryIdentity is Rust's re-export of mj-core's RepositoryIdentity.
type RepositoryIdentity = core.RepositoryIdentity

// rootIdentity reads a root's identity with local Git reads with no network
// or prompt, so no deadline: a fixed one failed imports on a loaded host
// while Git was still answering.
func rootIdentity(root string) (core.RepositoryIdentity, error) {
	return core.LocalIdentity(root, &core.ProcessExecutor{})
}

// ConfiguredRepositoryIdentity is a configured repository's identity; nil
// when it names no usable source.
func ConfiguredRepositoryIdentity(repository *core.ProjectRepository) (core.RepositoryIdentity, error) {
	if repository.Github != nil {
		return core.RepositoryIdentityFromRemote(*repository.Github), nil
	}
	if repository.Local != nil {
		return rootIdentity(*repository.Local)
	}
	return nil, nil
}

// ConfiguredRepositoryIdentityForLookup reads an identity while searching
// configured bundles. A stale local source makes its bundle ineligible for
// reuse, but must not prevent unrelated bundles from being considered.
func ConfiguredRepositoryIdentityForLookup(bundleID string, repository *core.ProjectRepository) core.RepositoryIdentity {
	identity, err := ConfiguredRepositoryIdentity(repository)
	if err == nil {
		return identity
	}
	path := "<not configured>"
	if repository.Local != nil {
		path = *repository.Local
	}
	slog.Warn("skipping configured repository whose identity cannot be read",
		"bundle_id", bundleID, "repository_id", repository.ID, "path", path, "error", err.Error())
	return nil
}

// ResolveBundle reuses an exact configured bundle or synthesizes one from all
// detected roots. requestedBundle is nil when the user named none.
func ResolveBundle(
	config *core.Config,
	cwd string,
	targets *SessionEditTargets,
	requestedBundle *string,
) (BundleResolution, error) {
	panic("unported: mj-import/src/import/bundles.rs resolve_bundle")
}

// BundleMatches reports whether a configured bundle holds exactly the
// detected identities (a set, Rust's BTreeSet) with primary as its primary
// repository.
func BundleMatches(
	bundleID string,
	bundle *core.ProjectBundle,
	detected map[core.RepositoryIdentity]struct{},
	primary core.RepositoryIdentity,
) (bool, error) {
	panic("unported: mj-import/src/import/bundles.rs bundle_matches")
}

// ConfiguredBundleForOrigin returns the matching configured bundle for an
// origin. It accepts setup's `owner/repository` shorthand as well as normal
// GitHub remote URLs.
func ConfiguredBundleForOrigin(config *core.Config, origin *GithubRepository) (string, bool) {
	panic("unported: mj-import/src/import/bundles.rs configured_bundle_for_origin")
}

// ConfiguredBundleForLocal returns the single-repository configured bundle
// whose local source is the repository at local.
func ConfiguredBundleForLocal(config *core.Config, local string) (string, bool) {
	panic("unported: mj-import/src/import/bundles.rs configured_bundle_for_local")
}

func sameGithubRepository(left, right *GithubRepository) bool {
	return importEqualFoldASCII(left.Owner, right.Owner) &&
		importEqualFoldASCII(left.Repository, right.Repository)
}

// importEqualFoldASCII is Rust's `str::eq_ignore_ascii_case`.
func importEqualFoldASCII(left, right string) bool {
	if len(left) != len(right) {
		return false
	}
	for i := 0; i < len(left); i++ {
		a, b := left[i], right[i]
		if 'A' <= a && a <= 'Z' {
			a += 'a' - 'A'
		}
		if 'A' <= b && b <= 'Z' {
			b += 'a' - 'A'
		}
		if a != b {
			return false
		}
	}
	return true
}

// UniqueBundleID is base, or the first `import-<base>` and
// `import-<base>-<n>` the config does not use.
func UniqueBundleID(config *core.Config, base string) string {
	panic("unported: mj-import/src/import/bundles.rs unique_bundle_id")
}
