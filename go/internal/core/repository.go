// Port of mj-core/src/repository.rs.

// Repository identity is independent of a checkout's location and push settings.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
)

// ProjectBundleSnapshot is a session's accepted project shape. Catalog
// merges change its project ID, not repository IDs, checkout layout, or
// source settings already accepted.
type ProjectBundleSnapshot struct {
	Bundle         ProjectBundle                 `json:"bundle"`
	Identities     map[string]RepositoryIdentity `json:"identities"`
	NetworkSources map[string]NetworkGitSource   `json:"network_sources"`
	// Repository IDs whose source is a non-snapshot directory rather than a
	// committed Git checkout. A bare local target accepts one; it is fixed
	// here at acceptance so a later `git init` or first commit in that folder
	// cannot silently change what the session's checkpoints and resumes
	// mean. A sorted set (Rust BTreeSet).
	PlainDirectories []string `json:"plain_directories,omitempty"`
}

func (s *ProjectBundleSnapshot) UnmarshalJSON(data []byte) error {
	var wire struct {
		Bundle           ProjectBundle               `json:"bundle"`
		Identities       map[string]json.RawMessage  `json:"identities"`
		NetworkSources   map[string]NetworkGitSource `json:"network_sources"`
		PlainDirectories []string                    `json:"plain_directories"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	identities := make(map[string]RepositoryIdentity, len(wire.Identities))
	for id, raw := range wire.Identities {
		identity, err := unmarshalRepositoryIdentity(raw)
		if err != nil {
			return err
		}
		identities[id] = identity
	}
	*s = ProjectBundleSnapshot{
		Bundle:           wire.Bundle,
		Identities:       identities,
		NetworkSources:   wire.NetworkSources,
		PlainDirectories: wire.PlainDirectories,
	}
	return nil
}

// PrimaryIsPlainDirectory is whether the primary project source was accepted
// as a directory that checkpoints leave in place rather than a committed Git
// checkout.
func (s *ProjectBundleSnapshot) PrimaryIsPlainDirectory() bool {
	for _, id := range s.PlainDirectories {
		if id == s.Bundle.PrimaryRepo {
			return true
		}
	}
	return false
}

func (s *ProjectBundleSnapshot) Key() (string, error) {
	panic("unported: mj-core/src/repository.rs ProjectBundleSnapshot::key")
}

func (s *ProjectBundleSnapshot) SourceKey() (string, error) {
	panic("unported: mj-core/src/repository.rs ProjectBundleSnapshot::source_key")
}

func (s *ProjectBundleSnapshot) Name() string {
	panic("unported: mj-core/src/repository.rs ProjectBundleSnapshot::name")
}

// RepositoryIdentity is a sealed interface over RepositoryIdentityGithub,
// RepositoryIdentityNetwork, RepositoryIdentityLocal and
// RepositoryIdentityRemoteDirectory. A nil value means Rust's None. It
// serializes as serde's externally tagged, snake_case enum.
type RepositoryIdentity interface {
	isRepositoryIdentity()
	Key() string
	Name() string
}

type RepositoryIdentityGithub struct {
	Owner      string
	Repository string
}

type RepositoryIdentityNetwork struct {
	URL string
}

type RepositoryIdentityLocal struct {
	Root string
}

type RepositoryIdentityRemoteDirectory struct {
	Host string `json:"host"`
	Root string `json:"root"`
}

func (RepositoryIdentityGithub) isRepositoryIdentity()          {}
func (RepositoryIdentityNetwork) isRepositoryIdentity()         {}
func (RepositoryIdentityLocal) isRepositoryIdentity()           {}
func (RepositoryIdentityRemoteDirectory) isRepositoryIdentity() {}

func (i RepositoryIdentityGithub) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string][2]string{"github": {i.Owner, i.Repository}})
}

func (i RepositoryIdentityNetwork) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"network": i.URL})
}

func (i RepositoryIdentityLocal) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"local": i.Root})
}

func (i RepositoryIdentityRemoteDirectory) MarshalJSON() ([]byte, error) {
	type wire RepositoryIdentityRemoteDirectory
	return json.Marshal(map[string]wire{"remote_directory": wire(i)})
}

// unmarshalRepositoryIdentity decodes serde's externally tagged form.
func unmarshalRepositoryIdentity(data []byte) (RepositoryIdentity, error) {
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return nil, err
	}
	if len(tagged) != 1 {
		return nil, errors.New("invalid type: expected a RepositoryIdentity map with a single key")
	}
	for variant, content := range tagged {
		switch variant {
		case "github":
			var pair [2]string
			if err := json.Unmarshal(content, &pair); err != nil {
				return nil, err
			}
			return RepositoryIdentityGithub{Owner: pair[0], Repository: pair[1]}, nil
		case "network":
			var url string
			if err := json.Unmarshal(content, &url); err != nil {
				return nil, err
			}
			return RepositoryIdentityNetwork{URL: url}, nil
		case "local":
			var root string
			if err := json.Unmarshal(content, &root); err != nil {
				return nil, err
			}
			return RepositoryIdentityLocal{Root: root}, nil
		case "remote_directory":
			type wire RepositoryIdentityRemoteDirectory
			var directory wire
			if err := json.Unmarshal(content, &directory); err != nil {
				return nil, err
			}
			return RepositoryIdentityRemoteDirectory(directory), nil
		default:
			return nil, fmt.Errorf("unknown variant `%s`, expected one of `github`, `network`, `local`, `remote_directory`", variant)
		}
	}
	panic("unreachable")
}

// CompareRepositoryIdentity is the Rust derived Ord: variant order, then
// fields (paths compare by component).
func CompareRepositoryIdentity(a, b RepositoryIdentity) int {
	panic("unported: mj-core/src/repository.rs RepositoryIdentity Ord")
}

// RepositoryIdentityFromRemote normalizes only equivalences guaranteed by
// the remote provider. Other servers can have case-sensitive paths or
// different transport namespaces.
func RepositoryIdentityFromRemote(source string) RepositoryIdentity {
	return RepositoryIdentityFromRemoteWith(source, ShorthandAllowed)
}

func RepositoryIdentityFromRemoteWith(source string, shorthand Shorthand) RepositoryIdentity {
	identity, _, ok := repositoryIdentityAndLabel(source, shorthand)
	if !ok {
		return nil
	}
	return identity
}

// repositoryIdentityAndLabel is the identity and display label of a Git
// remote. `shorthand` says whether a bare `owner/repository` means GitHub or
// is a relative path, which a network remote cannot be. Only GitHub's main
// endpoint is a Github identity; the same repository over `ssh.github.com`
// or an explicit SSH port stays Network, because stored identities
// distinguish them.
func repositoryIdentityAndLabel(source string, shorthand Shorthand) (RepositoryIdentity, string, bool) {
	panic("unported: mj-core/src/repository.rs RepositoryIdentity::identity_and_label")
}

func RepositoryIdentityRemoteLabel(source string) (string, bool) {
	return RepositoryIdentityRemoteLabelWith(source, ShorthandAllowed)
}

func RepositoryIdentityRemoteLabelWith(source string, shorthand Shorthand) (string, bool) {
	panic("unported: mj-core/src/repository.rs RepositoryIdentity::remote_label_with")
}

func (i RepositoryIdentityGithub) Key() string  { return "github:" + i.Owner + "/" + i.Repository }
func (i RepositoryIdentityNetwork) Key() string { return "git:" + i.URL }
func (i RepositoryIdentityLocal) Key() string   { return "path:" + i.Root }
func (i RepositoryIdentityRemoteDirectory) Key() string {
	return "path:" + i.Host + ":" + i.Root
}

func (i RepositoryIdentityGithub) Name() string { return i.Repository }
func (i RepositoryIdentityNetwork) Name() string {
	panic("unported: mj-core/src/repository.rs RepositoryIdentity::name (Network)")
}
func (i RepositoryIdentityLocal) Name() string {
	panic("unported: mj-core/src/repository.rs RepositoryIdentity::name (Local)")
}
func (i RepositoryIdentityRemoteDirectory) Name() string {
	panic("unported: mj-core/src/repository.rs RepositoryIdentity::name (RemoteDirectory)")
}

// LocalIdentity resolves identity without using the current feature branch
// or altering Git configuration. The caller's executor bounds any
// advertised-HEAD request.
func LocalIdentity(path string, executor CommandExecutor) (RepositoryIdentity, error) {
	resolved, err := ResolveDirectory(path, nil, executor)
	if err != nil {
		return nil, err
	}
	return resolved.Identity, nil
}

type ResolvedDirectory struct {
	CheckoutRoot   string
	RepositoryRoot string
	Identity       RepositoryIdentity
	// The directory has no committed Git checkout. Only a bare local target
	// accepts one; see ResolveLocalDirectory.
	PlainDirectory bool
}

// RepositoryUnavailable is expected absence when discovering historical
// directories. Other resolver failures still indicate an error that
// discovery must report and retry. A sealed interface over
// RepositoryUnavailableDirectory and RepositoryUnavailableNotRepository.
type RepositoryUnavailable interface {
	error
	isRepositoryUnavailable()
}

type RepositoryUnavailableDirectory struct{ Path string }

type RepositoryUnavailableNotRepository struct{ Path string }

func (RepositoryUnavailableDirectory) isRepositoryUnavailable()     {}
func (RepositoryUnavailableNotRepository) isRepositoryUnavailable() {}

func (e RepositoryUnavailableDirectory) Error() string {
	return fmt.Sprintf("project directory %s is unavailable", e.Path)
}

func (e RepositoryUnavailableNotRepository) Error() string {
	return fmt.Sprintf("%s is not a Git repository", e.Path)
}

// ResolveDirectory: the executor can run on the controller or translate Git
// commands to SSH. Preserve the selected checkout while collapsing its
// identity to the owner of the common Git directory.
func ResolveDirectory(path string, host *string, executor CommandExecutor) (ResolvedDirectory, error) {
	roots, err := resolveDirectoryRoots(path, host, executor)
	if err != nil {
		return ResolvedDirectory{}, err
	}
	identity, err := directoryIdentity(path, host, roots.repositoryRoot, executor)
	if err != nil {
		return ResolvedDirectory{}, err
	}
	return ResolvedDirectory{
		CheckoutRoot:   roots.checkoutRoot,
		RepositoryRoot: roots.repositoryRoot,
		Identity:       identity,
	}, nil
}

type directoryRoots struct {
	checkoutRoot   string
	repositoryRoot string
}

func resolveDirectoryRoots(path string, host *string, executor CommandExecutor) (directoryRoots, error) {
	panic("unported: mj-core/src/repository.rs resolve_directory_roots")
}

func directoryIdentity(path string, host *string, repositoryRoot string, executor CommandExecutor) (RepositoryIdentity, error) {
	panic("unported: mj-core/src/repository.rs directory_identity")
}

// ResolveLocalDirectory resolves a directory a bare local session may run in.
//
// A local directory that is not a committed Git checkout is still a project
// the user can open: its identity is its canonical path, it has no checkout
// above it, and nothing about it can be cloned. This includes an ordinary
// folder, a host with no Git executable, and a repository that has not made
// its first commit yet. Git failures that could hide unreadable repository
// state remain errors. Only a local target qualifies; a remote directory
// still has to be a repository because it is addressed by the repository it
// shares with its sessions.
func ResolveLocalDirectory(path string, executor CommandExecutor) (ResolvedDirectory, error) {
	panic("unported: mj-core/src/repository.rs resolve_local_directory")
}

func plainLocalDirectory(path string) (ResolvedDirectory, error) {
	panic("unported: mj-core/src/repository.rs plain_local_directory")
}

func commandNotFound(err error) bool {
	panic("unported: mj-core/src/repository.rs command_not_found")
}
