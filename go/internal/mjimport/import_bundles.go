// Port of mj-import/src/import/bundles.rs.

package mjimport

import (
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"math"
	"slices"
	"unicode/utf8"

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
	cwdRoot, ok, err := GitRootForPath(cwd)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, errors.New("session cwd is not in a Git worktree")
	}
	// A linked worktree stands in for its main repository, so bundles are
	// named after and point at the main working tree.
	cwdRoot, err = core.MainWorktreeRoot(cwdRoot)
	if err != nil {
		return nil, err
	}
	primaryIdentity, err := rootIdentity(cwdRoot)
	if err != nil {
		return nil, err
	}
	detected := map[core.RepositoryIdentity]struct{}{}
	for _, root := range targets.GitRoots {
		identity, err := rootIdentity(root)
		if err != nil {
			return nil, err
		}
		detected[identity] = struct{}{}
	}

	if requestedBundle != nil {
		bundleID := *requestedBundle
		bundle, ok := config.Bundles[bundleID]
		if !ok {
			return nil, fmt.Errorf("unknown bundle %s", core.RustDebugString(bundleID))
		}
		matches, err := BundleMatches(bundleID, &bundle, detected, primaryIdentity)
		if err != nil {
			return nil, err
		}
		if !matches {
			return nil, fmt.Errorf("bundle %s does not exactly match the session's edited Git roots and cwd primary repository",
				core.RustDebugString(bundleID))
		}
		return BundleResolutionExisting(bundleID), nil
	}
	for _, id := range slices.Sorted(maps.Keys(config.Bundles)) {
		bundle := config.Bundles[id]
		matches, err := BundleMatches(id, &bundle, detected, primaryIdentity)
		if err != nil {
			return nil, err
		}
		if matches {
			return BundleResolutionExisting(id), nil
		}
	}

	primaryName := bundleRootName(cwdRoot)
	bundleID := UniqueBundleID(config, SetupStyleID(primaryName))
	usedIDs := map[string]struct{}{}
	repositories := []core.ProjectRepository{}
	var primaryRepo *string
	roots := make([]string, 0, len(targets.GitRoots))
	for _, root := range targets.GitRoots {
		main, err := core.MainWorktreeRoot(root)
		if err != nil {
			return nil, err
		}
		roots = append(roots, main)
	}
	// The cwd repository sorts first, the rest keep their order.
	slices.SortStableFunc(roots, func(left, right string) int {
		leftOther := importComparePaths(left, cwdRoot) != 0
		rightOther := importComparePaths(right, cwdRoot) != 0
		switch {
		case leftOther == rightOther:
			return 0
		case !leftOther:
			return -1
		default:
			return 1
		}
	})
	// Checkouts and worktrees of one repository share an identity. Keep the
	// first root per identity so the cwd repository stays primary.
	usedIdentities := map[core.RepositoryIdentity]struct{}{}
	for _, root := range roots {
		identity, err := rootIdentity(root)
		if err != nil {
			return nil, err
		}
		if _, used := usedIdentities[identity]; used {
			continue
		}
		usedIdentities[identity] = struct{}{}
		base := SetupStyleID(bundleRootName(root))
		id := base
		for suffix := 2; ; suffix++ {
			if _, used := usedIDs[id]; !used {
				usedIDs[id] = struct{}{}
				break
			}
			id = fmt.Sprintf("%s-%d", base, suffix)
		}
		if importComparePaths(root, cwdRoot) == 0 {
			primary := id
			primaryRepo = &primary
		}
		local := root
		repositories = append(repositories, core.ProjectRepository{
			ID:           id,
			Local:        &local,
			Github:       nil,
			Destination:  id,
			GitRef:       nil,
			MbxNamespace: nil,
		})
	}
	if primaryRepo == nil {
		return nil, errors.New("detected roots omitted the cwd repository")
	}
	return BundleResolutionSynthesized{
		ID:     bundleID,
		Bundle: core.ProjectBundle{PrimaryRepo: *primaryRepo, Repositories: repositories},
	}, nil
}

// bundleRootName is a root's directory name, `repository` when it has none.
func bundleRootName(root string) string {
	if name, ok := importPathFileName(root); ok && utf8.ValidString(name) {
		return name
	}
	return "repository"
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
	identities := make([]core.RepositoryIdentity, 0, len(bundle.Repositories))
	for index := range bundle.Repositories {
		identity := ConfiguredRepositoryIdentityForLookup(bundleID, &bundle.Repositories[index])
		if identity == nil {
			return false, nil
		}
		identities = append(identities, identity)
	}
	identitySet := map[core.RepositoryIdentity]struct{}{}
	for _, identity := range identities {
		identitySet[identity] = struct{}{}
	}
	primaryIndex := slices.IndexFunc(bundle.Repositories, func(repository core.ProjectRepository) bool {
		return repository.ID == bundle.PrimaryRepo
	})
	if primaryIndex < 0 {
		return false, nil
	}
	if len(identitySet) != len(bundle.Repositories) || len(identitySet) != len(detected) {
		return false, nil
	}
	for identity := range identitySet {
		if _, ok := detected[identity]; !ok {
			return false, nil
		}
	}
	return identities[primaryIndex] == primary, nil
}

// ConfiguredBundleForOrigin returns the matching configured bundle for an
// origin. It accepts setup's `owner/repository` shorthand as well as normal
// GitHub remote URLs.
func ConfiguredBundleForOrigin(config *core.Config, origin *GithubRepository) (string, bool) {
	for _, id := range slices.Sorted(maps.Keys(config.Bundles)) {
		bundle := config.Bundles[id]
		if len(bundle.Repositories) != 1 {
			continue
		}
		primary := bundle.Primary()
		if primary == nil || primary.Github == nil {
			continue
		}
		configured := GithubRepositoryFromOrigin(*primary.Github)
		if configured != nil && sameGithubRepository(configured, origin) {
			return id, true
		}
	}
	return "", false
}

// ConfiguredBundleForLocal returns the single-repository configured bundle
// whose local source is the repository at local.
func ConfiguredBundleForLocal(config *core.Config, local string) (string, bool) {
	local, err := core.CanonicalRepository(local)
	if err != nil {
		return "", false
	}
	for _, id := range slices.Sorted(maps.Keys(config.Bundles)) {
		bundle := config.Bundles[id]
		if len(bundle.Repositories) != 1 {
			continue
		}
		primary := bundle.Primary()
		if primary == nil || primary.Local == nil {
			continue
		}
		configured, err := core.CanonicalRepository(*primary.Local)
		if err != nil {
			continue
		}
		if importComparePaths(configured, local) == 0 {
			return id, true
		}
	}
	return "", false
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
	if _, used := config.Bundles[base]; !used {
		return base
	}
	base = "import-" + base
	if _, used := config.Bundles[base]; !used {
		return base
	}
	for suffix := uint64(2); suffix <= math.MaxUint32; suffix++ {
		candidate := fmt.Sprintf("%s-%d", base, suffix)
		if _, used := config.Bundles[candidate]; !used {
			return candidate
		}
	}
	panic("u32 bundle suffixes are finite")
}
